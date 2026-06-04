//! Bundle/domain event chains backed by CAS objects and signed refs.

use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::objects::{
    hash_domain_event, validate_domain_identifier, DomainEventAttribution, DomainEventObject,
    DOMAIN_EVENT_KIND, SCHEMA_VERSION,
};
use crate::refs;
use crate::signer::Signer;

const DOMAIN_EVENTS_NAMESPACE: &str = "domain_events";
const MAX_DOMAIN_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct DomainEventAppendRequest {
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
    pub attribution: DomainEventAttribution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainEventAppendResult {
    pub event_hash: String,
    pub chain_head_hash: String,
    pub event: DomainEventObject,
    pub idempotent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainEventRecord {
    pub event_hash: String,
    pub event: DomainEventObject,
}

struct DomainEventChainLock {
    _lock_file: File,
}

impl DomainEventChainLock {
    fn acquire(
        refs_root: &Path,
        bundle_id: &str,
        event_kind: &str,
        chain_id: &str,
    ) -> anyhow::Result<Self> {
        let lock_path = refs_root
            .join("generic")
            .join(DOMAIN_EVENTS_NAMESPACE)
            .join(bundle_id)
            .join(event_kind)
            .join("chains")
            .join(chain_id)
            .join("lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).context("failed to create domain event lock dir")?;
        }
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| {
                format!("failed to open domain event lock: {}", lock_path.display())
            })?;

        #[cfg(unix)]
        {
            let ret = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
            if ret != 0 {
                anyhow::bail!(
                    "domain event flock failed at {}: {}",
                    lock_path.display(),
                    std::io::Error::last_os_error()
                );
            }
        }

        Ok(Self {
            _lock_file: lock_file,
        })
    }
}

impl Drop for DomainEventChainLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            let _ = libc::flock(self._lock_file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[tracing::instrument(
    name = "state:domain_event_append",
    skip(cas_root, refs_root, request, signer),
    fields(
        effective_bundle_id = %request.effective_bundle_id,
        event_kind = %request.event_kind,
        chain_id = %request.chain_id,
        event_type = %request.event_type,
    )
)]
pub fn append_domain_event(
    cas_root: &Path,
    refs_root: &Path,
    request: DomainEventAppendRequest,
    signer: &dyn Signer,
) -> anyhow::Result<DomainEventAppendResult> {
    let bundle_id = request
        .bundle_id
        .as_deref()
        .unwrap_or(&request.effective_bundle_id)
        .to_string();
    if bundle_id != request.effective_bundle_id {
        anyhow::bail!(
            "domain event bundle_id mismatch: requested {}, effective {}",
            bundle_id,
            request.effective_bundle_id
        );
    }

    validate_append_request(&bundle_id, &request)?;
    let _lock = DomainEventChainLock::acquire(
        refs_root,
        &bundle_id,
        &request.event_kind,
        &request.chain_id,
    )?;

    let request_fingerprint = compute_request_fingerprint(&bundle_id, &request);
    if let Some(result) = maybe_return_idempotent(
        cas_root,
        refs_root,
        &bundle_id,
        &request,
        &request_fingerprint,
        signer,
    )? {
        return Ok(result);
    }

    let current_head = refs::read_generic_head_ref(
        refs_root,
        DOMAIN_EVENTS_NAMESPACE,
        &chain_ref_name(&bundle_id, &request.event_kind, &request.chain_id),
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
        let previous = read_domain_event_by_hash(cas_root, head_hash)?;
        (previous.event.chain_seq + 1, Some(head_hash.to_string()))
    } else {
        (1, None)
    };

    let event = DomainEventObject {
        schema: SCHEMA_VERSION,
        kind: DOMAIN_EVENT_KIND.to_string(),
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
    let event_json = lillux::canonical_json(&event.to_value());
    let event_hash = lillux::sha256_hex(event_json.as_bytes());
    let event_path = lillux::shard_path(cas_root, "objects", &event_hash, ".json");
    lillux::atomic_write(&event_path, event_json.as_bytes())
        .context("failed to store domain event in CAS")?;

    refs::write_generic_head_ref(
        refs_root,
        DOMAIN_EVENTS_NAMESPACE,
        &chain_ref_name(&bundle_id, &event.event_kind, &event.chain_id),
        &event_hash,
        signer,
    )
    .context("failed to write domain event chain head")?;

    if let Some(idempotency_key) = &event.idempotency_key {
        refs::write_generic_head_ref(
            refs_root,
            DOMAIN_EVENTS_NAMESPACE,
            &idempotency_ref_name(
                &bundle_id,
                &event.event_kind,
                &event.chain_id,
                idempotency_key,
            ),
            &event_hash,
            signer,
        )
        .context("failed to write domain event idempotency head")?;
    }

    Ok(DomainEventAppendResult {
        event_hash: event_hash.clone(),
        chain_head_hash: event_hash,
        event,
        idempotent: false,
    })
}

pub fn read_domain_event_chain(
    cas_root: &Path,
    refs_root: &Path,
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
) -> anyhow::Result<Vec<DomainEventRecord>> {
    validate_domain_identifier("bundle_id", bundle_id)?;
    validate_domain_identifier("event_kind", event_kind)?;
    validate_domain_identifier("chain_id", chain_id)?;
    let Some(head) = refs::read_generic_head_ref(
        refs_root,
        DOMAIN_EVENTS_NAMESPACE,
        &chain_ref_name(bundle_id, event_kind, chain_id),
    )?
    else {
        return Ok(Vec::new());
    };

    let mut records = Vec::new();
    let mut next_hash = Some(head.target_hash);
    while let Some(hash) = next_hash {
        let record = read_domain_event_by_hash(cas_root, &hash)?;
        if record.event.bundle_id != bundle_id
            || record.event.event_kind != event_kind
            || record.event.chain_id != chain_id
        {
            anyhow::bail!("domain event chain contains mismatched event metadata");
        }
        next_hash = record.event.prev_chain_event_hash.clone();
        records.push(record);
    }
    records.reverse();
    validate_domain_event_chain_links(bundle_id, event_kind, chain_id, &records)?;
    Ok(records)
}

pub fn scan_domain_events(
    cas_root: &Path,
    refs_root: &Path,
    bundle_id: &str,
    event_kind: &str,
) -> anyhow::Result<Vec<DomainEventRecord>> {
    validate_domain_identifier("bundle_id", bundle_id)?;
    validate_domain_identifier("event_kind", event_kind)?;
    let prefix = format!(
        "{}/{}/{}/chains",
        DOMAIN_EVENTS_NAMESPACE, bundle_id, event_kind
    );
    let heads = refs::list_generic_head_refs(refs_root, &prefix)?;
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
        records.extend(read_domain_event_chain(
            cas_root, refs_root, bundle_id, event_kind, parts[3],
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

fn validate_domain_event_chain_links(
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
    records: &[DomainEventRecord],
) -> anyhow::Result<()> {
    for (idx, record) in records.iter().enumerate() {
        let expected_seq = (idx + 1) as u64;
        if record.event.chain_seq != expected_seq {
            anyhow::bail!(
                "domain event chain {}/{}/{} has sequence gap: expected {}, got {}",
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
                "domain event chain {}/{}/{} has link mismatch at seq {}: expected prev {:?}, got {:?}",
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

pub fn read_domain_event_by_hash(
    cas_root: &Path,
    event_hash: &str,
) -> anyhow::Result<DomainEventRecord> {
    validate_canonical_hash("event_hash", event_hash)?;
    let path = lillux::shard_path(cas_root, "objects", event_hash, ".json");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read domain event object {}", path.display()))?;
    let event: DomainEventObject = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse domain event {}", event_hash))?;
    event.validate()?;
    let actual_hash = hash_domain_event(&event);
    if actual_hash != event_hash {
        anyhow::bail!(
            "domain event hash mismatch: expected {}, got {}",
            event_hash,
            actual_hash
        );
    }
    Ok(DomainEventRecord {
        event_hash: event_hash.to_string(),
        event,
    })
}

fn maybe_return_idempotent(
    cas_root: &Path,
    refs_root: &Path,
    bundle_id: &str,
    request: &DomainEventAppendRequest,
    request_fingerprint: &str,
    signer: &dyn Signer,
) -> anyhow::Result<Option<DomainEventAppendResult>> {
    let Some(idempotency_key) = &request.idempotency_key else {
        return Ok(None);
    };
    if let Some(existing_ref) = refs::read_generic_head_ref(
        refs_root,
        DOMAIN_EVENTS_NAMESPACE,
        &idempotency_ref_name(
            bundle_id,
            &request.event_kind,
            &request.chain_id,
            idempotency_key,
        ),
    )? {
        let existing = read_domain_event_by_hash(cas_root, &existing_ref.target_hash)?;
        return idempotent_result_or_conflict(
            refs_root,
            bundle_id,
            request,
            request_fingerprint,
            existing,
            None,
        );
    }

    if let Some(existing) = find_idempotent_event_in_chain(
        cas_root,
        refs_root,
        bundle_id,
        &request.event_kind,
        &request.chain_id,
        idempotency_key,
    )? {
        return idempotent_result_or_conflict(
            refs_root,
            bundle_id,
            request,
            request_fingerprint,
            existing,
            Some(signer),
        );
    }

    Ok(None)
}

fn find_idempotent_event_in_chain(
    cas_root: &Path,
    refs_root: &Path,
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
    idempotency_key: &str,
) -> anyhow::Result<Option<DomainEventRecord>> {
    for record in read_domain_event_chain(cas_root, refs_root, bundle_id, event_kind, chain_id)? {
        if record.event.idempotency_key.as_deref() == Some(idempotency_key) {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

fn idempotent_result_or_conflict(
    refs_root: &Path,
    bundle_id: &str,
    request: &DomainEventAppendRequest,
    request_fingerprint: &str,
    existing: DomainEventRecord,
    repair_signer: Option<&dyn Signer>,
) -> anyhow::Result<Option<DomainEventAppendResult>> {
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
            refs::write_generic_head_ref(
                refs_root,
                DOMAIN_EVENTS_NAMESPACE,
                &idempotency_ref_name(
                    bundle_id,
                    &existing.event.event_kind,
                    &existing.event.chain_id,
                    idempotency_key,
                ),
                &existing.event_hash,
                signer,
            )
            .context("failed to repair domain event idempotency head")?;
        }
    }
    let chain_head_hash =
        current_chain_head_hash(refs_root, bundle_id, &request.event_kind, &request.chain_id)?
            .unwrap_or_else(|| existing.event_hash.clone());
    Ok(Some(DomainEventAppendResult {
        event_hash: existing.event_hash,
        chain_head_hash,
        event: existing.event,
        idempotent: true,
    }))
}

fn current_chain_head_hash(
    refs_root: &Path,
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
) -> anyhow::Result<Option<String>> {
    Ok(refs::read_generic_head_ref(
        refs_root,
        DOMAIN_EVENTS_NAMESPACE,
        &chain_ref_name(bundle_id, event_kind, chain_id),
    )?
    .map(|head| head.target_hash))
}

fn validate_append_request(
    bundle_id: &str,
    request: &DomainEventAppendRequest,
) -> anyhow::Result<()> {
    validate_domain_identifier("bundle_id", bundle_id)?;
    validate_domain_identifier("event_kind", &request.event_kind)?;
    validate_domain_identifier("event_type", &request.event_type)?;
    validate_domain_identifier("chain_id", &request.chain_id)?;
    if request.schema_version == 0 {
        anyhow::bail!("schema_version must be greater than zero");
    }
    validate_payload_size(&request.payload)?;
    if let Some(hash) = &request.expected_chain_head_hash {
        validate_canonical_hash("expected_chain_head_hash", hash)?;
    }
    if let Some(key) = &request.idempotency_key {
        crate::objects::domain_event::validate_idempotency_key(key)?;
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

fn compute_request_fingerprint(bundle_id: &str, request: &DomainEventAppendRequest) -> String {
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
    if bytes > MAX_DOMAIN_EVENT_PAYLOAD_BYTES {
        anyhow::bail!(
            "domain event payload too large: {} > {}",
            bytes,
            MAX_DOMAIN_EVENT_PAYLOAD_BYTES
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

    fn append_request(chain_id: &str, event_type: &str) -> DomainEventAppendRequest {
        DomainEventAppendRequest {
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
            attribution: DomainEventAttribution::default(),
        }
    }

    #[test]
    fn appends_and_reads_domain_event_chain() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();

        let first = append_domain_event(
            &cas_root,
            &refs_root,
            append_request("email_1", "email_planned"),
            &signer,
        )
        .unwrap();
        let mut second_req = append_request("email_1", "email_approved");
        second_req.expected_chain_head_hash = Some(first.event_hash.clone());
        let second = append_domain_event(&cas_root, &refs_root, second_req, &signer).unwrap();

        let chain = read_domain_event_chain(
            &cas_root,
            &refs_root,
            "ryeos-email",
            "email_event",
            "email_1",
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

        append_domain_event(
            &cas_root,
            &refs_root,
            append_request("email_1", "email_planned"),
            &signer,
        )
        .unwrap();
        let err = append_domain_event(
            &cas_root,
            &refs_root,
            append_request("email_1", "email_approved"),
            &signer,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("StaleHead"));
    }

    #[test]
    fn duplicate_idempotency_returns_original_and_conflict_on_payload_change() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();

        let mut req = append_request("email_1", "email_send_requested");
        req.idempotency_key = Some("request-send:email_1".to_string());
        let first = append_domain_event(&cas_root, &refs_root, req.clone(), &signer).unwrap();

        let retry = append_domain_event(&cas_root, &refs_root, req.clone(), &signer).unwrap();
        assert!(retry.idempotent);
        assert_eq!(retry.event_hash, first.event_hash);

        req.payload = serde_json::json!({"email_id":"email_1","changed":true});
        let err = append_domain_event(&cas_root, &refs_root, req, &signer).unwrap_err();
        assert!(format!("{err:#}").contains("IdempotencyConflict"));
    }

    #[test]
    fn missing_idempotency_ref_is_repaired_by_scanning_chain() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();

        let mut req = append_request("email_1", "email_send_requested");
        req.idempotency_key = Some("request-send:email_1".to_string());
        let first = append_domain_event(&cas_root, &refs_root, req.clone(), &signer).unwrap();
        let idem_path = refs_root
            .join("generic")
            .join(DOMAIN_EVENTS_NAMESPACE)
            .join(idempotency_ref_name(
                "ryeos-email",
                "email_event",
                "email_1",
                "request-send:email_1",
            ))
            .join("head");
        assert!(idem_path.is_file());
        std::fs::remove_file(&idem_path).unwrap();

        let retry = append_domain_event(&cas_root, &refs_root, req, &signer).unwrap();
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

        let mut first_req = append_request("email_1", "email_send_requested");
        first_req.idempotency_key = Some("request-send:email_1".to_string());
        let first = append_domain_event(&cas_root, &refs_root, first_req.clone(), &signer).unwrap();

        let mut second_req = append_request("email_1", "email_send_claimed");
        second_req.expected_chain_head_hash = Some(first.event_hash.clone());
        let second = append_domain_event(&cas_root, &refs_root, second_req, &signer).unwrap();

        let retry = append_domain_event(&cas_root, &refs_root, first_req, &signer).unwrap();
        assert!(retry.idempotent);
        assert_eq!(retry.event_hash, first.event_hash);
        assert_eq!(retry.chain_head_hash, second.event_hash);
    }

    #[test]
    fn scan_order_is_deterministic_across_chains() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();

        append_domain_event(
            &cas_root,
            &refs_root,
            append_request("email_b", "email_planned"),
            &signer,
        )
        .unwrap();
        append_domain_event(
            &cas_root,
            &refs_root,
            append_request("email_a", "email_planned"),
            &signer,
        )
        .unwrap();

        let scanned =
            scan_domain_events(&cas_root, &refs_root, "ryeos-email", "email_event").unwrap();
        assert_eq!(scanned.len(), 2);
        assert_eq!(scanned[0].event.chain_id, "email_a");
        assert_eq!(scanned[1].event.chain_id, "email_b");
    }

    #[test]
    fn read_chain_rejects_sequence_or_link_mismatch() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();

        let first = append_domain_event(
            &cas_root,
            &refs_root,
            append_request("email_1", "email_planned"),
            &signer,
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
        refs::write_generic_head_ref(
            &refs_root,
            DOMAIN_EVENTS_NAMESPACE,
            &chain_ref_name("ryeos-email", "email_event", "email_1"),
            &malformed_hash,
            &signer,
        )
        .unwrap();

        let err = read_domain_event_chain(
            &cas_root,
            &refs_root,
            "ryeos-email",
            "email_event",
            "email_1",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("sequence gap"));
    }

    #[test]
    fn caller_cannot_spoof_bundle_id() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();
        let mut req = append_request("email_1", "email_planned");
        req.bundle_id = Some("other-bundle".to_string());
        let err = append_domain_event(&cas_root, &refs_root, req, &signer).unwrap_err();
        assert!(format!("{err:#}").contains("bundle_id mismatch"));
    }
}
