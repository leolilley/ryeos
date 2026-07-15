//! Bundle event chains backed by CAS objects and signed refs.

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::objects::{
    hash_bundle_event, validate_bundle_identifier, BundleEventAttribution, BundleEventObject,
    BUNDLE_EVENT_KIND, SCHEMA_VERSION,
};
use crate::refs;
use crate::signer::Signer;

const BUNDLE_EVENTS_NAMESPACE: &str = "bundle_events";
const MAX_BUNDLE_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;

fn pin_bundle_event_authority(
    cas_root: &Path,
    refs_root: &Path,
) -> anyhow::Result<(
    lillux::PinnedDirectory,
    lillux::CasStore,
    lillux::PinnedDirectory,
)> {
    let runtime_path = cas_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("CAS root has no runtime-state parent"))?;
    if refs_root.parent() != Some(runtime_path) {
        anyhow::bail!("CAS and refs roots do not share one runtime-state parent");
    }
    let runtime = lillux::PinnedDirectory::open(runtime_path)?
        .ok_or_else(|| anyhow::anyhow!("runtime-state directory is absent"))?;
    let cas_directory = runtime
        .open_child_directory(std::ffi::OsStr::new("objects"))?
        .ok_or_else(|| anyhow::anyhow!("CAS root is absent"))?;
    let refs_directory = runtime
        .open_child_directory(std::ffi::OsStr::new("refs"))?
        .ok_or_else(|| anyhow::anyhow!("refs root is absent"))?;
    Ok((
        runtime,
        lillux::CasStore::from_pinned_root(cas_directory),
        refs_directory,
    ))
}

#[derive(Debug, Clone)]
pub struct BundleEventAppendRequest {
    pub effective_bundle_id: String,
    pub bundle_id: Option<String>,
    pub event_kind: String,
    pub chain_id: String,
    pub event_type: String,
    pub schema_version: u32,
    pub payload: serde_json::Value,
    pub expected_chain_head_hash: Option<String>,
    pub idempotency_key: Option<String>,
    pub correlation_id: Option<String>,
    pub causation_id: Option<String>,
    pub attribution: BundleEventAttribution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleEventAppendResult {
    pub event_hash: String,
    pub chain_head_hash: String,
    pub event: BundleEventObject,
    pub idempotent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleEventRecord {
    pub event_hash: String,
    pub event: BundleEventObject,
}

#[tracing::instrument(
    name = "state:bundle_event_append",
    skip(cas_root, refs_root, request, signer, trust_store),
    fields(
        effective_bundle_id = %request.effective_bundle_id,
        event_kind = %request.event_kind,
        chain_id = %request.chain_id,
        event_type = %request.event_type,
    )
)]
pub(crate) fn append_bundle_event(
    cas_root: &Path,
    refs_root: &Path,
    request: BundleEventAppendRequest,
    signer: &dyn Signer,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<BundleEventAppendResult> {
    let cas_guard = crate::recovery::CasMutationGuard::shared_from_cas_root(cas_root)?;
    append_bundle_event_admitted(
        cas_root,
        refs_root,
        request,
        signer,
        trust_store,
        &cas_guard,
    )
}

pub(crate) fn append_bundle_event_admitted(
    cas_root: &Path,
    refs_root: &Path,
    request: BundleEventAppendRequest,
    signer: &dyn Signer,
    trust_store: &refs::TrustStore,
    cas_guard: &crate::recovery::CasMutationGuard,
) -> anyhow::Result<BundleEventAppendResult> {
    cas_guard.ensure_protects_cas_root(cas_root)?;
    let (runtime, cas, refs_directory) = pin_bundle_event_authority(cas_root, refs_root)?;
    cas_guard.ensure_protects_pinned_runtime(&runtime)?;
    append_bundle_event_admitted_pinned(&cas, &refs_directory, request, signer, trust_store)
}

pub(crate) fn append_bundle_event_admitted_pinned(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    request: BundleEventAppendRequest,
    signer: &dyn Signer,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<BundleEventAppendResult> {
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    let bundle_id = request
        .bundle_id
        .as_deref()
        .unwrap_or(&request.effective_bundle_id)
        .to_string();
    if bundle_id != request.effective_bundle_id {
        anyhow::bail!(
            "bundle event bundle_id mismatch: requested {}, effective {}",
            bundle_id,
            request.effective_bundle_id
        );
    }

    validate_append_request(&bundle_id, &request)?;
    let chain_head_name = chain_ref_name(&bundle_id, &request.event_kind, &request.chain_id);
    let chain_lock = refs::GenericHeadLock::acquire_in_refs_directory(
        refs_directory,
        BUNDLE_EVENTS_NAMESPACE,
        &chain_head_name,
    )?;

    let request_fingerprint = compute_request_fingerprint(&bundle_id, &request);
    if let Some(result) = maybe_return_idempotent(
        cas,
        refs_directory,
        &bundle_id,
        &request,
        &request_fingerprint,
        signer,
        trust_store,
    )? {
        return Ok(result);
    }

    let current_head = refs::read_verified_generic_head_ref_in_directory(
        refs_directory,
        BUNDLE_EVENTS_NAMESPACE,
        &chain_head_name,
        trust_store,
    )?;
    let current_head_hash = current_head.as_ref().map(|head| head.target_hash.as_str());
    if current_head_hash != request.expected_chain_head_hash.as_deref() {
        anyhow::bail!(
            "StaleHead for bundle/event_kind/chain {}/{}/{}: expected {:?}, current {:?}",
            bundle_id,
            request.event_kind,
            request.chain_id,
            request.expected_chain_head_hash,
            current_head_hash
        );
    }

    let (chain_seq, prev_chain_event_hash) = if let Some(head_hash) = current_head_hash {
        let previous = read_bundle_event_by_hash_with_cas(cas, head_hash)?;
        (previous.event.chain_seq + 1, Some(head_hash.to_string()))
    } else {
        (1, None)
    };

    let event = BundleEventObject {
        schema: SCHEMA_VERSION,
        kind: BUNDLE_EVENT_KIND.to_string(),
        bundle_id: bundle_id.clone(),
        event_kind: request.event_kind.clone(),
        event_type: request.event_type.clone(),
        schema_version: request.schema_version,
        chain_id: request.chain_id.clone(),
        chain_seq,
        prev_chain_event_hash,
        created_at: lillux::time::iso8601_now(),
        attribution: request.attribution,
        idempotency_key: request.idempotency_key.clone(),
        request_fingerprint: Some(request_fingerprint),
        correlation_id: request.correlation_id,
        causation_id: request.causation_id,
        payload: request.payload,
    };
    event.validate()?;
    let event_value = event.to_value();
    let expected_event_hash = hash_bundle_event(&event);
    let stored = cas
        .put_object(&event_value)
        .context("failed to store bundle event in CAS")?;
    if stored.hash != expected_event_hash {
        anyhow::bail!(
            "bundle event CAS hash mismatch: expected {}, got {}",
            expected_event_hash,
            stored.hash
        );
    }
    let event_hash = stored.hash;

    refs::advance_verified_generic_head_ref_in_directory(
        refs_directory,
        BUNDLE_EVENTS_NAMESPACE,
        &chain_head_name,
        &event_hash,
        current_head_hash,
        signer,
        trust_store,
        &chain_lock,
    )
    .context("failed to write bundle event chain head")?;

    if let Some(idempotency_key) = &event.idempotency_key {
        let idempotency_name = idempotency_ref_name(
            &bundle_id,
            &event.event_kind,
            &event.chain_id,
            idempotency_key,
        );
        let idempotency_lock = refs::GenericHeadLock::acquire_in_refs_directory(
            refs_directory,
            BUNDLE_EVENTS_NAMESPACE,
            &idempotency_name,
        )?;
        refs::write_verified_generic_head_ref_in_directory(
            refs_directory,
            BUNDLE_EVENTS_NAMESPACE,
            &idempotency_name,
            &event_hash,
            signer,
            trust_store,
            &idempotency_lock,
        )
        .context("failed to write bundle event idempotency head")?;
    }

    Ok(BundleEventAppendResult {
        event_hash: event_hash.clone(),
        chain_head_hash: event_hash,
        event,
        idempotent: false,
    })
}

pub(crate) fn read_bundle_event_chain(
    cas_root: &Path,
    refs_root: &Path,
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<Vec<BundleEventRecord>> {
    let (_runtime, cas, refs_directory) = pin_bundle_event_authority(cas_root, refs_root)?;
    read_bundle_event_chain_pinned(
        &cas,
        &refs_directory,
        bundle_id,
        event_kind,
        chain_id,
        trust_store,
    )
}

pub(crate) fn read_bundle_event_chain_pinned(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<Vec<BundleEventRecord>> {
    validate_bundle_identifier("bundle_id", bundle_id)?;
    validate_bundle_identifier("event_kind", event_kind)?;
    validate_bundle_identifier("chain_id", chain_id)?;
    let Some(head) = refs::read_verified_generic_head_ref_in_directory(
        refs_directory,
        BUNDLE_EVENTS_NAMESPACE,
        &chain_ref_name(bundle_id, event_kind, chain_id),
        trust_store,
    )?
    else {
        return Ok(Vec::new());
    };

    read_bundle_event_chain_from_head(cas, bundle_id, event_kind, chain_id, &head.target_hash)
}

fn read_bundle_event_chain_from_head(
    cas: &lillux::CasStore,
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
    head_hash: &str,
) -> anyhow::Result<Vec<BundleEventRecord>> {
    let mut records = Vec::new();
    let mut next_hash = Some(head_hash.to_string());
    while let Some(hash) = next_hash {
        let record = read_bundle_event_by_hash_with_cas(cas, &hash)?;
        if record.event.bundle_id != bundle_id
            || record.event.event_kind != event_kind
            || record.event.chain_id != chain_id
        {
            anyhow::bail!("bundle event chain contains mismatched event metadata");
        }
        next_hash = record.event.prev_chain_event_hash.clone();
        records.push(record);
    }
    records.reverse();
    validate_bundle_event_chain_links(bundle_id, event_kind, chain_id, &records)?;
    Ok(records)
}

pub(crate) fn scan_bundle_events(
    cas_root: &Path,
    refs_root: &Path,
    bundle_id: &str,
    event_kind: &str,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<Vec<BundleEventRecord>> {
    let (_runtime, cas, refs_directory) = pin_bundle_event_authority(cas_root, refs_root)?;
    scan_bundle_events_pinned(&cas, &refs_directory, bundle_id, event_kind, trust_store)
}

pub(crate) fn scan_bundle_events_pinned(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    bundle_id: &str,
    event_kind: &str,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<Vec<BundleEventRecord>> {
    validate_bundle_identifier("bundle_id", bundle_id)?;
    validate_bundle_identifier("event_kind", event_kind)?;
    let prefix = format!(
        "{}/{}/{}/chains",
        BUNDLE_EVENTS_NAMESPACE, bundle_id, event_kind
    );
    let heads =
        refs::list_verified_generic_head_refs_in_directory(refs_directory, &prefix, trust_store)?;
    let mut records = Vec::new();
    for head in heads {
        let parts: Vec<_> = head.name.split('/').collect();
        if parts.len() != 4
            || parts[0] != bundle_id
            || parts[1] != event_kind
            || parts[2] != "chains"
        {
            continue;
        }
        records.extend(read_bundle_event_chain_from_head(
            cas,
            bundle_id,
            event_kind,
            parts[3],
            &head.target_hash,
        )?);
    }
    records.sort_by(|a, b| {
        (
            &a.event.bundle_id,
            &a.event.event_kind,
            &a.event.chain_id,
            a.event.chain_seq,
            &a.event_hash,
        )
            .cmp(&(
                &b.event.bundle_id,
                &b.event.event_kind,
                &b.event.chain_id,
                b.event.chain_seq,
                &b.event_hash,
            ))
    });
    Ok(records)
}

fn validate_bundle_event_chain_links(
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
    records: &[BundleEventRecord],
) -> anyhow::Result<()> {
    for (idx, record) in records.iter().enumerate() {
        let expected_seq = (idx + 1) as u64;
        if record.event.chain_seq != expected_seq {
            anyhow::bail!(
                "bundle event chain {}/{}/{} has sequence gap: expected {}, got {}",
                bundle_id,
                event_kind,
                chain_id,
                expected_seq,
                record.event.chain_seq
            );
        }

        let expected_prev = idx
            .checked_sub(1)
            .map(|prev_idx| records[prev_idx].event_hash.as_str());
        if record.event.prev_chain_event_hash.as_deref() != expected_prev {
            anyhow::bail!(
                "bundle event chain {}/{}/{} has link mismatch at seq {}: expected prev {:?}, got {:?}",
                bundle_id,
                event_kind,
                chain_id,
                record.event.chain_seq,
                expected_prev,
                record.event.prev_chain_event_hash
            );
        }
    }
    Ok(())
}

pub fn read_bundle_event_by_hash(
    cas_root: &Path,
    event_hash: &str,
) -> anyhow::Result<BundleEventRecord> {
    let cas = lillux::CasStore::new(cas_root.to_path_buf());
    read_bundle_event_by_hash_with_cas(&cas, event_hash)
}

fn read_bundle_event_by_hash_with_cas(
    cas: &lillux::CasStore,
    event_hash: &str,
) -> anyhow::Result<BundleEventRecord> {
    validate_canonical_hash("event_hash", event_hash)?;
    let value = cas
        .get_object(event_hash)
        .with_context(|| format!("failed to read bundle event object {event_hash}"))?
        .ok_or_else(|| anyhow::anyhow!("bundle event object {event_hash} is missing"))?;
    let event: BundleEventObject = serde_json::from_value(value)
        .with_context(|| format!("failed to parse bundle event {}", event_hash))?;
    event.validate()?;
    let actual_hash = hash_bundle_event(&event);
    if actual_hash != event_hash {
        anyhow::bail!(
            "bundle event hash mismatch: expected {}, got {}",
            event_hash,
            actual_hash
        );
    }
    Ok(BundleEventRecord {
        event_hash: event_hash.to_string(),
        event,
    })
}

fn maybe_return_idempotent(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    bundle_id: &str,
    request: &BundleEventAppendRequest,
    request_fingerprint: &str,
    signer: &dyn Signer,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<Option<BundleEventAppendResult>> {
    let Some(idempotency_key) = &request.idempotency_key else {
        return Ok(None);
    };
    if let Some(existing_ref) = refs::read_verified_generic_head_ref_in_directory(
        refs_directory,
        BUNDLE_EVENTS_NAMESPACE,
        &idempotency_ref_name(
            bundle_id,
            &request.event_kind,
            &request.chain_id,
            idempotency_key,
        ),
        trust_store,
    )? {
        let existing = read_bundle_event_by_hash_with_cas(cas, &existing_ref.target_hash)?;
        return idempotent_result_or_conflict(
            refs_directory,
            bundle_id,
            request,
            request_fingerprint,
            existing,
            None,
            trust_store,
        );
    }

    if let Some(existing) = find_idempotent_event_in_chain(
        cas,
        refs_directory,
        bundle_id,
        &request.event_kind,
        &request.chain_id,
        idempotency_key,
        trust_store,
    )? {
        return idempotent_result_or_conflict(
            refs_directory,
            bundle_id,
            request,
            request_fingerprint,
            existing,
            Some(signer),
            trust_store,
        );
    }

    Ok(None)
}

fn find_idempotent_event_in_chain(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
    idempotency_key: &str,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<Option<BundleEventRecord>> {
    for record in read_bundle_event_chain_pinned(
        cas,
        refs_directory,
        bundle_id,
        event_kind,
        chain_id,
        trust_store,
    )? {
        if record.event.idempotency_key.as_deref() == Some(idempotency_key) {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

fn idempotent_result_or_conflict(
    refs_directory: &lillux::PinnedDirectory,
    bundle_id: &str,
    request: &BundleEventAppendRequest,
    request_fingerprint: &str,
    existing: BundleEventRecord,
    repair_signer: Option<&dyn Signer>,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<Option<BundleEventAppendResult>> {
    if existing.event.request_fingerprint.as_deref() != Some(request_fingerprint) {
        anyhow::bail!(
            "IdempotencyConflict for bundle/event_kind/chain {}/{}/{}",
            bundle_id,
            request.event_kind,
            request.chain_id
        );
    }
    if let Some(signer) = repair_signer {
        if let Some(idempotency_key) = &existing.event.idempotency_key {
            let idempotency_name = idempotency_ref_name(
                bundle_id,
                &existing.event.event_kind,
                &existing.event.chain_id,
                idempotency_key,
            );
            let idempotency_lock = refs::GenericHeadLock::acquire_in_refs_directory(
                refs_directory,
                BUNDLE_EVENTS_NAMESPACE,
                &idempotency_name,
            )?;
            refs::write_verified_generic_head_ref_in_directory(
                refs_directory,
                BUNDLE_EVENTS_NAMESPACE,
                &idempotency_name,
                &existing.event_hash,
                signer,
                trust_store,
                &idempotency_lock,
            )
            .context("failed to repair bundle event idempotency head")?;
        }
    }
    let chain_head_hash = current_chain_head_hash(
        refs_directory,
        bundle_id,
        &request.event_kind,
        &request.chain_id,
        trust_store,
    )?
    .unwrap_or_else(|| existing.event_hash.clone());
    Ok(Some(BundleEventAppendResult {
        event_hash: existing.event_hash,
        chain_head_hash,
        event: existing.event,
        idempotent: true,
    }))
}

fn current_chain_head_hash(
    refs_directory: &lillux::PinnedDirectory,
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<Option<String>> {
    Ok(refs::read_verified_generic_head_ref_in_directory(
        refs_directory,
        BUNDLE_EVENTS_NAMESPACE,
        &chain_ref_name(bundle_id, event_kind, chain_id),
        trust_store,
    )?
    .map(|head| head.target_hash))
}

fn validate_append_request(
    bundle_id: &str,
    request: &BundleEventAppendRequest,
) -> anyhow::Result<()> {
    validate_bundle_identifier("bundle_id", bundle_id)?;
    validate_bundle_identifier("event_kind", &request.event_kind)?;
    validate_bundle_identifier("event_type", &request.event_type)?;
    validate_bundle_identifier("chain_id", &request.chain_id)?;
    if request.schema_version == 0 {
        anyhow::bail!("schema_version must be greater than zero");
    }
    validate_payload_size(&request.payload)?;
    if let Some(hash) = &request.expected_chain_head_hash {
        validate_canonical_hash("expected_chain_head_hash", hash)?;
    }
    if let Some(key) = &request.idempotency_key {
        crate::objects::bundle_event::validate_idempotency_key(key)?;
    }
    Ok(())
}

fn chain_ref_name(bundle_id: &str, event_kind: &str, chain_id: &str) -> String {
    format!("{}/{}/chains/{}", bundle_id, event_kind, chain_id)
}

fn idempotency_ref_name(
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
    idempotency_key: &str,
) -> String {
    let key_hash = lillux::sha256_hex(
        format!(
            "{}\0{}\0{}\0{}",
            bundle_id, event_kind, chain_id, idempotency_key
        )
        .as_bytes(),
    );
    format!("{}/{}/idempotency/{}", bundle_id, event_kind, key_hash)
}

fn compute_request_fingerprint(bundle_id: &str, request: &BundleEventAppendRequest) -> String {
    let value = serde_json::json!({
        "bundle_id": bundle_id,
        "event_kind": request.event_kind,
        "chain_id": request.chain_id,
        "event_type": request.event_type,
        "schema_version": request.schema_version,
        "payload": request.payload,
        "idempotency_key": request.idempotency_key,
        "correlation_id": request.correlation_id,
        "causation_id": request.causation_id,
    });
    lillux::sha256_hex(lillux::canonical_json(&value).as_bytes())
}

fn validate_payload_size(payload: &serde_json::Value) -> anyhow::Result<()> {
    let bytes = lillux::canonical_json(payload).len();
    if bytes > MAX_BUNDLE_EVENT_PAYLOAD_BYTES {
        anyhow::bail!(
            "bundle event payload too large: {} > {}",
            bytes,
            MAX_BUNDLE_EVENT_PAYLOAD_BYTES
        );
    }
    Ok(())
}

fn validate_canonical_hash(label: &str, hash: &str) -> anyhow::Result<()> {
    if !lillux::valid_hash(hash) || hash.bytes().any(|b| b.is_ascii_uppercase()) {
        anyhow::bail!("invalid {label}: {hash}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;

    fn roots() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();
        (tmp, cas_root, refs_root)
    }

    fn append_request(chain_id: &str, event_type: &str) -> BundleEventAppendRequest {
        BundleEventAppendRequest {
            effective_bundle_id: "ryeos-email".to_string(),
            bundle_id: Some("ryeos-email".to_string()),
            event_kind: "email_event".to_string(),
            chain_id: chain_id.to_string(),
            event_type: event_type.to_string(),
            schema_version: 1,
            payload: serde_json::json!({"email_id": chain_id}),
            expected_chain_head_hash: None,
            idempotency_key: None,
            correlation_id: None,
            causation_id: None,
            attribution: BundleEventAttribution::default(),
        }
    }

    fn trust_store(signer: &TestSigner) -> refs::TrustStore {
        let mut trust = refs::TrustStore::new();
        trust.insert(signer.fingerprint().to_string(), signer.verifying_key());
        trust
    }

    #[test]
    fn appends_and_reads_bundle_event_chain() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();
        let trust = trust_store(&signer);

        let first = append_bundle_event(
            &cas_root,
            &refs_root,
            append_request("email_1", "email_planned"),
            &signer,
            &trust,
        )
        .unwrap();
        let mut second_req = append_request("email_1", "email_approved");
        second_req.expected_chain_head_hash = Some(first.event_hash.clone());
        let second =
            append_bundle_event(&cas_root, &refs_root, second_req, &signer, &trust).unwrap();

        let chain = read_bundle_event_chain(
            &cas_root,
            &refs_root,
            "ryeos-email",
            "email_event",
            "email_1",
            &trust,
        )
        .unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].event_hash, first.event_hash);
        assert_eq!(chain[1].event_hash, second.event_hash);
        assert_eq!(
            chain[1].event.prev_chain_event_hash.as_deref(),
            Some(chain[0].event_hash.as_str())
        );
        assert_eq!(chain[1].event.chain_seq, 2);
    }

    #[test]
    fn stale_expected_head_is_rejected() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();
        let trust = trust_store(&signer);

        append_bundle_event(
            &cas_root,
            &refs_root,
            append_request("email_1", "email_planned"),
            &signer,
            &trust,
        )
        .unwrap();
        let err = append_bundle_event(
            &cas_root,
            &refs_root,
            append_request("email_1", "email_approved"),
            &signer,
            &trust,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("StaleHead"));
    }

    #[test]
    fn duplicate_idempotency_returns_original_and_conflict_on_payload_change() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();
        let trust = trust_store(&signer);

        let mut req = append_request("email_1", "email_send_requested");
        req.idempotency_key = Some("request-send:email_1".to_string());
        let first =
            append_bundle_event(&cas_root, &refs_root, req.clone(), &signer, &trust).unwrap();

        let retry =
            append_bundle_event(&cas_root, &refs_root, req.clone(), &signer, &trust).unwrap();
        assert!(retry.idempotent);
        assert_eq!(retry.event_hash, first.event_hash);

        req.payload = serde_json::json!({"email_id":"email_1","changed":true});
        let err = append_bundle_event(&cas_root, &refs_root, req, &signer, &trust).unwrap_err();
        assert!(format!("{err:#}").contains("IdempotencyConflict"));
    }

    #[test]
    fn missing_idempotency_ref_is_repaired_by_scanning_chain() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();
        let trust = trust_store(&signer);

        let mut req = append_request("email_1", "email_send_requested");
        req.idempotency_key = Some("request-send:email_1".to_string());
        let first =
            append_bundle_event(&cas_root, &refs_root, req.clone(), &signer, &trust).unwrap();
        let idem_path = refs_root
            .join("generic")
            .join(BUNDLE_EVENTS_NAMESPACE)
            .join(idempotency_ref_name(
                "ryeos-email",
                "email_event",
                "email_1",
                "request-send:email_1",
            ))
            .join("head");
        assert!(idem_path.is_file());
        std::fs::remove_file(&idem_path).unwrap();

        let retry = append_bundle_event(&cas_root, &refs_root, req, &signer, &trust).unwrap();
        assert!(retry.idempotent);
        assert_eq!(retry.event_hash, first.event_hash);
        assert!(
            idem_path.is_file(),
            "retry should repair missing idempotency ref"
        );
    }

    #[test]
    fn idempotent_retry_reports_current_chain_head_after_later_append() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();
        let trust = trust_store(&signer);

        let mut first_req = append_request("email_1", "email_send_requested");
        first_req.idempotency_key = Some("request-send:email_1".to_string());
        let first =
            append_bundle_event(&cas_root, &refs_root, first_req.clone(), &signer, &trust).unwrap();

        let mut second_req = append_request("email_1", "email_send_claimed");
        second_req.expected_chain_head_hash = Some(first.event_hash.clone());
        let second =
            append_bundle_event(&cas_root, &refs_root, second_req, &signer, &trust).unwrap();

        let retry = append_bundle_event(&cas_root, &refs_root, first_req, &signer, &trust).unwrap();
        assert!(retry.idempotent);
        assert_eq!(retry.event_hash, first.event_hash);
        assert_eq!(retry.chain_head_hash, second.event_hash);
    }

    #[test]
    fn scan_order_is_deterministic_across_chains() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();
        let trust = trust_store(&signer);

        append_bundle_event(
            &cas_root,
            &refs_root,
            append_request("email_b", "email_planned"),
            &signer,
            &trust,
        )
        .unwrap();
        append_bundle_event(
            &cas_root,
            &refs_root,
            append_request("email_a", "email_planned"),
            &signer,
            &trust,
        )
        .unwrap();

        let scanned =
            scan_bundle_events(&cas_root, &refs_root, "ryeos-email", "email_event", &trust)
                .unwrap();
        assert_eq!(scanned.len(), 2);
        assert_eq!(scanned[0].event.chain_id, "email_a");
        assert_eq!(scanned[1].event.chain_id, "email_b");
    }

    #[test]
    fn read_chain_rejects_sequence_or_link_mismatch() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();
        let trust = trust_store(&signer);

        let first = append_bundle_event(
            &cas_root,
            &refs_root,
            append_request("email_1", "email_planned"),
            &signer,
            &trust,
        )
        .unwrap();
        let mut malformed = first.event.clone();
        malformed.event_type = "email_malformed".to_string();
        malformed.chain_seq = 2;
        malformed.prev_chain_event_hash = None;
        malformed.created_at = lillux::time::iso8601_now();
        let malformed_json = lillux::canonical_json(&malformed.to_value());
        let malformed_hash = lillux::sha256_hex(malformed_json.as_bytes());
        let malformed_path = lillux::shard_path(&cas_root, "objects", &malformed_hash, ".json");
        lillux::atomic_write(&malformed_path, malformed_json.as_bytes()).unwrap();
        let head_name = chain_ref_name("ryeos-email", "email_event", "email_1");
        let head_lock =
            refs::GenericHeadLock::acquire(&refs_root, BUNDLE_EVENTS_NAMESPACE, &head_name)
                .unwrap();
        refs::advance_verified_generic_head_ref(
            &refs_root,
            BUNDLE_EVENTS_NAMESPACE,
            &head_name,
            &malformed_hash,
            Some(&first.event_hash),
            &signer,
            &trust,
            &head_lock,
        )
        .unwrap();

        let err = read_bundle_event_chain(
            &cas_root,
            &refs_root,
            "ryeos-email",
            "email_event",
            "email_1",
            &trust,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("sequence gap"));
    }

    #[test]
    fn caller_cannot_spoof_bundle_id() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();
        let trust = trust_store(&signer);
        let mut req = append_request("email_1", "email_planned");
        req.bundle_id = Some("other-bundle".to_string());
        let err = append_bundle_event(&cas_root, &refs_root, req, &signer, &trust).unwrap_err();
        assert!(format!("{err:#}").contains("bundle_id mismatch"));
    }
}
