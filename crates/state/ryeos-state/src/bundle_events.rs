//! Bundle event chains backed by CAS objects and signed refs.

use std::path::Path;

use anyhow::Context;
use base64::Engine as _;
use lillux::crypto::Verifier as _;
use serde::{Deserialize, Serialize};

use crate::objects::{
    hash_bundle_event, validate_bundle_identifier, BundleEventAttachment, BundleEventAttribution,
    BundleEventObject, BUNDLE_EVENT_KIND, MAX_BUNDLE_EVENT_SERIALIZED_BYTES, SCHEMA_VERSION,
};
use crate::refs;
use crate::signer::Signer;

const BUNDLE_EVENTS_NAMESPACE: &str = "bundle_events";
const MAX_BUNDLE_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;
/// Maximum filesystem entries a single paged cross-chain scan may inspect while
/// selecting the next lexicographic chain. `read_dir` order is unspecified, so
/// correctness requires examining the whole directory; rejecting an oversized
/// namespace gives that operation a hard CPU/syscall bound until chain heads
/// gain an indexed ordering structure.
pub const MAX_BUNDLE_EVENT_SCAN_INSPECTED_ENTRIES: usize = 4_096;

#[cfg(test)]
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
    pub attachments: Vec<BundleEventAttachment>,
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

/// A newest-first page from one bundle event chain.
#[derive(Debug, Clone)]
pub struct BundleEventChainPage {
    pub records: Vec<BundleEventRecord>,
    pub next_cursor: Option<BundleEventCursor>,
}

/// Signed, identity-bound keyset cursor for bundle-event pagination.
///
/// The cursor names an event reachable from one verified chain head. Any head
/// advance makes the cursor stale, preventing callers from substituting an
/// arbitrary CAS object or continuing across a changed authoritative view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEventCursor {
    pub schema: u32,
    pub kind: String,
    pub bundle_id: String,
    pub event_kind: String,
    pub chain_id: String,
    pub head_hash: String,
    pub event_hash: String,
    pub signer: String,
    pub signature: String,
}

/// A newest-first page from one chain in a cross-chain scan. Chains are
/// visited in lexicographic `chain_id` order.
#[derive(Debug, Clone)]
pub struct BundleEventScanPage {
    pub records: Vec<BundleEventRecord>,
    pub next_cursor: Option<BundleEventCursor>,
}

#[derive(Debug)]
struct BundleEventHashPage {
    records: Vec<BundleEventRecord>,
    next_hash: Option<String>,
}

const BUNDLE_EVENT_CURSOR_KIND: &str = "bundle_event_cursor";

impl BundleEventCursor {
    fn new(
        bundle_id: &str,
        event_kind: &str,
        chain_id: &str,
        head_hash: &str,
        event_hash: &str,
        signer: &dyn Signer,
        trust_store: &refs::TrustStore,
    ) -> anyhow::Result<Self> {
        crate::signer::ensure_signer_trusted(signer, trust_store)?;
        let mut cursor = Self {
            schema: SCHEMA_VERSION,
            kind: BUNDLE_EVENT_CURSOR_KIND.to_string(),
            bundle_id: bundle_id.to_string(),
            event_kind: event_kind.to_string(),
            chain_id: chain_id.to_string(),
            head_hash: head_hash.to_string(),
            event_hash: event_hash.to_string(),
            signer: signer.fingerprint().to_string(),
            signature: String::new(),
        };
        cursor.validate_structure(false)?;
        let canonical = cursor.canonical_unsigned()?;
        cursor.signature =
            base64::engine::general_purpose::STANDARD.encode(signer.sign(canonical.as_bytes()));
        cursor.verify(bundle_id, event_kind, trust_store)?;
        Ok(cursor)
    }

    fn verify(
        &self,
        bundle_id: &str,
        event_kind: &str,
        trust_store: &refs::TrustStore,
    ) -> anyhow::Result<()> {
        self.validate_structure(true)?;
        if self.bundle_id != bundle_id || self.event_kind != event_kind {
            anyhow::bail!(
                "bundle event cursor identity mismatch: expected {}/{}, got {}/{}",
                bundle_id,
                event_kind,
                self.bundle_id,
                self.event_kind
            );
        }
        let verifying_key = trust_store
            .get(&self.signer)
            .ok_or_else(|| anyhow::anyhow!("bundle event cursor signer is not trusted"))?;
        let signature_bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.signature)
            .context("failed to decode bundle event cursor signature")?;
        let signature =
            lillux::crypto::Signature::from_slice(&signature_bytes).map_err(|error| {
                anyhow::anyhow!("failed to parse bundle event cursor signature: {error}")
            })?;
        let canonical = self.canonical_unsigned()?;
        verifying_key
            .verify(canonical.as_bytes(), &signature)
            .map_err(|error| {
                anyhow::anyhow!("bundle event cursor signature verification failed: {error}")
            })
    }

    fn validate_structure(&self, require_signature: bool) -> anyhow::Result<()> {
        if self.schema != SCHEMA_VERSION {
            anyhow::bail!("unsupported bundle event cursor schema: {}", self.schema);
        }
        if self.kind != BUNDLE_EVENT_CURSOR_KIND {
            anyhow::bail!("invalid bundle event cursor kind: {}", self.kind);
        }
        validate_bundle_identifier("cursor bundle_id", &self.bundle_id)?;
        validate_bundle_identifier("cursor event_kind", &self.event_kind)?;
        validate_bundle_identifier("cursor chain_id", &self.chain_id)?;
        validate_canonical_hash("cursor head_hash", &self.head_hash)?;
        validate_canonical_hash("cursor event_hash", &self.event_hash)?;
        validate_canonical_hash("cursor signer", &self.signer)?;
        if require_signature && self.signature.is_empty() {
            anyhow::bail!("bundle event cursor signature must not be empty");
        }
        if self.signature.len() > 128 {
            anyhow::bail!("bundle event cursor signature is too long");
        }
        Ok(())
    }

    fn canonical_unsigned(&self) -> anyhow::Result<String> {
        lillux::canonical_json(&serde_json::json!({
            "schema": self.schema,
            "kind": self.kind,
            "bundle_id": self.bundle_id,
            "event_kind": self.event_kind,
            "chain_id": self.chain_id,
            "head_hash": self.head_hash,
            "event_hash": self.event_hash,
            "signer": self.signer,
        }))
        .context("canonicalize unsigned bundle event cursor")
    }
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
#[cfg(test)]
pub(crate) fn append_bundle_event(
    cas_root: &Path,
    refs_root: &Path,
    request: BundleEventAppendRequest,
    signer: &dyn Signer,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<BundleEventAppendResult> {
    let cas_guard = crate::recovery::CasMutationGuard::shared_from_cas_root(cas_root)?;
    cas_guard.ensure_protects_cas_root(cas_root)?;
    let (runtime, cas, refs_directory) = pin_bundle_event_authority(cas_root, refs_root)?;
    cas_guard.ensure_protects_pinned_runtime(&runtime)?;
    append_bundle_event_pinned(&cas, &refs_directory, request, signer, trust_store)
}

pub(crate) fn append_bundle_event_pinned(
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

    let request_fingerprint = compute_request_fingerprint(&bundle_id, &request)?;
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
        attachments: request.attachments,
        payload: request.payload,
    };
    event.validate()?;
    let event_value = event.to_value();
    let expected_event_hash =
        hash_bundle_event(&event).context("failed to canonicalize bundle event")?;
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

#[cfg(test)]
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

#[cfg(test)]
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

/// Read a bounded, newest-first page from one chain.
// Keep each signed authority, cursor bound, and serialization limit explicit at
// this trust boundary; grouping them would hide which inputs have been verified.
#[allow(clippy::too_many_arguments)]
pub fn read_bundle_event_chain_page(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    trust_store: &refs::TrustStore,
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
    cursor: Option<&BundleEventCursor>,
    limit: usize,
    max_serialized_bytes: usize,
    signer: &dyn Signer,
) -> anyhow::Result<BundleEventChainPage> {
    validate_bundle_identifier("bundle_id", bundle_id)?;
    validate_bundle_identifier("event_kind", event_kind)?;
    validate_bundle_identifier("chain_id", chain_id)?;
    validate_bundle_event_page_bounds(limit, max_serialized_bytes)?;

    let current_head = refs::read_verified_generic_head_ref_in_directory(
        refs_directory,
        BUNDLE_EVENTS_NAMESPACE,
        &chain_ref_name(bundle_id, event_kind, chain_id),
        trust_store,
    )?;
    let Some(current_head) = current_head else {
        if cursor.is_some() {
            anyhow::bail!("bundle event cursor names a chain with no current head");
        }
        return Ok(BundleEventChainPage {
            records: Vec::new(),
            next_cursor: None,
        });
    };
    let head_hash = current_head.target_hash;
    let start_hash = match cursor {
        Some(cursor) => {
            cursor.verify(bundle_id, event_kind, trust_store)?;
            if cursor.chain_id != chain_id {
                anyhow::bail!(
                    "bundle event cursor chain mismatch: expected {}, got {}",
                    chain_id,
                    cursor.chain_id
                );
            }
            if cursor.head_hash != head_hash {
                anyhow::bail!(
                    "stale bundle event cursor for chain {}: anchored at {}, current head {}",
                    chain_id,
                    cursor.head_hash,
                    head_hash
                );
            }
            cursor.event_hash.clone()
        }
        None => head_hash.clone(),
    };

    let page = read_bundle_event_chain_page_from_hash(
        cas,
        bundle_id,
        event_kind,
        chain_id,
        Some(start_hash),
        limit,
        max_serialized_bytes,
    )?;
    let next_cursor = page
        .next_hash
        .as_deref()
        .map(|event_hash| {
            BundleEventCursor::new(
                bundle_id,
                event_kind,
                chain_id,
                &head_hash,
                event_hash,
                signer,
                trust_store,
            )
        })
        .transpose()?;
    Ok(BundleEventChainPage {
        records: page.records,
        next_cursor,
    })
}

/// Scan bounded pages across bundle event chains without collecting every
/// signed head or every event under the StateStore lock.
// Keep each signed authority, cursor bound, and serialization limit explicit at
// this trust boundary; grouping them would hide which inputs have been verified.
#[allow(clippy::too_many_arguments)]
pub fn scan_bundle_events_page(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    trust_store: &refs::TrustStore,
    bundle_id: &str,
    event_kind: &str,
    cursor: Option<&BundleEventCursor>,
    limit: usize,
    max_serialized_bytes: usize,
    signer: &dyn Signer,
) -> anyhow::Result<BundleEventScanPage> {
    validate_bundle_identifier("bundle_id", bundle_id)?;
    validate_bundle_identifier("event_kind", event_kind)?;
    validate_bundle_event_page_bounds(limit, max_serialized_bytes)?;

    let Some((chain_id, head_hash, start_hash)) = (match cursor {
        Some(cursor) => {
            cursor.verify(bundle_id, event_kind, trust_store)?;
            let current_head = refs::read_verified_generic_head_ref_in_directory(
                refs_directory,
                BUNDLE_EVENTS_NAMESPACE,
                &chain_ref_name(bundle_id, event_kind, &cursor.chain_id),
                trust_store,
            )?
            .ok_or_else(|| {
                anyhow::anyhow!("bundle event cursor names a chain with no current head")
            })?;
            if cursor.head_hash != current_head.target_hash {
                anyhow::bail!(
                    "stale bundle event cursor for chain {}: anchored at {}, current head {}",
                    cursor.chain_id,
                    cursor.head_hash,
                    current_head.target_hash
                );
            }
            Some((
                cursor.chain_id.clone(),
                cursor.head_hash.clone(),
                cursor.event_hash.clone(),
            ))
        }
        None => {
            next_bundle_event_chain_head(refs_directory, bundle_id, event_kind, None, trust_store)?
                .map(|(chain_id, head_hash)| (chain_id, head_hash.clone(), head_hash))
        }
    }) else {
        return Ok(BundleEventScanPage {
            records: Vec::new(),
            next_cursor: None,
        });
    };

    let page = read_bundle_event_chain_page_from_hash(
        cas,
        bundle_id,
        event_kind,
        &chain_id,
        Some(start_hash),
        limit,
        max_serialized_bytes,
    )?;

    let next_cursor = if let Some(event_hash) = page.next_hash {
        Some(BundleEventCursor::new(
            bundle_id,
            event_kind,
            &chain_id,
            &head_hash,
            &event_hash,
            signer,
            trust_store,
        )?)
    } else {
        next_bundle_event_chain_head(
            refs_directory,
            bundle_id,
            event_kind,
            Some(&chain_id),
            trust_store,
        )?
        .map(|(chain_id, head_hash)| {
            BundleEventCursor::new(
                bundle_id,
                event_kind,
                &chain_id,
                &head_hash,
                &head_hash,
                signer,
                trust_store,
            )
        })
        .transpose()?
    };

    Ok(BundleEventScanPage {
        records: page.records,
        next_cursor,
    })
}

fn read_bundle_event_chain_page_from_hash(
    cas: &lillux::CasStore,
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
    mut next_hash: Option<String>,
    limit: usize,
    max_serialized_bytes: usize,
) -> anyhow::Result<BundleEventHashPage> {
    let mut records = Vec::with_capacity(limit.min(64));
    let mut serialized_bytes = 0usize;
    let mut expected_seq = None;

    while records.len() < limit {
        let Some(hash) = next_hash.take() else {
            break;
        };
        let record = read_bundle_event_by_hash_with_cas(cas, &hash)?;
        if record.event.bundle_id != bundle_id
            || record.event.event_kind != event_kind
            || record.event.chain_id != chain_id
        {
            anyhow::bail!("bundle event chain cursor contains mismatched event metadata");
        }
        if let Some(expected_seq) = expected_seq {
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
        }
        match &record.event.prev_chain_event_hash {
            Some(_) if record.event.chain_seq == 1 => anyhow::bail!(
                "bundle event chain {}/{}/{} has a predecessor at sequence 1",
                bundle_id,
                event_kind,
                chain_id
            ),
            None if record.event.chain_seq != 1 => anyhow::bail!(
                "bundle event chain {}/{}/{} ends at sequence {}",
                bundle_id,
                event_kind,
                chain_id,
                record.event.chain_seq
            ),
            _ => {}
        }

        let record_bytes = serde_json::to_vec(&record)
            .context("failed to measure serialized bundle event record")?
            .len();
        let next_serialized_bytes = serialized_bytes
            .checked_add(record_bytes)
            .context("bundle event page serialized byte count overflow")?;
        if next_serialized_bytes > max_serialized_bytes {
            if records.is_empty() {
                anyhow::bail!(
                    "single bundle event requires {} serialized bytes (page budget {})",
                    record_bytes,
                    max_serialized_bytes
                );
            }
            next_hash = Some(hash);
            break;
        }

        expected_seq = record.event.chain_seq.checked_sub(1);
        next_hash = record.event.prev_chain_event_hash.clone();
        serialized_bytes = next_serialized_bytes;
        records.push(record);
    }

    Ok(BundleEventHashPage { records, next_hash })
}

fn next_bundle_event_chain_head(
    refs_directory: &lillux::PinnedDirectory,
    bundle_id: &str,
    event_kind: &str,
    after_chain_id: Option<&str>,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<Option<(String, String)>> {
    let mut chains_directory =
        match refs_directory.open_child_directory(std::ffi::OsStr::new("generic"))? {
            Some(directory) => directory,
            None => return Ok(None),
        };
    for component in [BUNDLE_EVENTS_NAMESPACE, bundle_id, event_kind, "chains"] {
        chains_directory =
            match chains_directory.open_child_directory(std::ffi::OsStr::new(component))? {
                Some(directory) => directory,
                None => return Ok(None),
            };
    }
    let entries =
        chains_directory.entry_names_bounded(MAX_BUNDLE_EVENT_SCAN_INSPECTED_ENTRIES + 1)?;
    if entries.len() > MAX_BUNDLE_EVENT_SCAN_INSPECTED_ENTRIES {
        anyhow::bail!(
            "bundle event scan exceeds the {}-entry chain directory inspection limit",
            MAX_BUNDLE_EVENT_SCAN_INSPECTED_ENTRIES
        );
    }

    // read_dir order is unspecified, so retain only the smallest eligible id
    // instead of collecting and sorting chain heads. The inspection ceiling is
    // intentionally independent of the response page size: an adversarial refs
    // directory must not make a one-record callback scan walk unbounded entries
    // while the StateStore lock is held.
    let mut next_chain_id: Option<String> = None;
    for entry in entries {
        chains_directory
            .open_child_directory(&entry)?
            .ok_or_else(|| anyhow::anyhow!("bundle event chain directory disappeared"))?;
        let chain_id = entry
            .into_string()
            .map_err(|_| anyhow::anyhow!("bundle event chain directory name is not valid UTF-8"))?;
        validate_bundle_identifier("chain_id", &chain_id)?;
        if let Some(after_chain_id) = after_chain_id {
            if chain_id.as_str() <= after_chain_id {
                continue;
            }
        }
        if match next_chain_id.as_ref() {
            Some(current) => chain_id < *current,
            None => true,
        } {
            next_chain_id = Some(chain_id);
        }
    }

    let Some(chain_id) = next_chain_id else {
        return Ok(None);
    };
    let head = refs::read_verified_generic_head_ref_in_directory(
        refs_directory,
        BUNDLE_EVENTS_NAMESPACE,
        &chain_ref_name(bundle_id, event_kind, &chain_id),
        trust_store,
    )?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "bundle event chain {}/{}/{} has no signed head",
            bundle_id,
            event_kind,
            chain_id
        )
    })?;
    Ok(Some((chain_id, head.target_hash)))
}

fn validate_bundle_event_page_bounds(
    limit: usize,
    max_serialized_bytes: usize,
) -> anyhow::Result<()> {
    if limit == 0 {
        anyhow::bail!("bundle event page limit must be greater than zero");
    }
    if max_serialized_bytes == 0 {
        anyhow::bail!("bundle event page byte budget must be greater than zero");
    }
    Ok(())
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

pub(crate) fn read_bundle_event_by_hash_with_cas(
    cas: &lillux::CasStore,
    event_hash: &str,
) -> anyhow::Result<BundleEventRecord> {
    validate_canonical_hash("event_hash", event_hash)?;
    let value = cas
        .get_object(event_hash)
        .with_context(|| format!("failed to read bundle event object {event_hash}"))?
        .ok_or_else(|| anyhow::anyhow!("bundle event object {event_hash} is missing"))?;
    let object_bytes = lillux::canonical_json(&value)
        .context("failed to canonicalize bundle event while checking its size")?
        .len();
    if object_bytes > MAX_BUNDLE_EVENT_SERIALIZED_BYTES {
        anyhow::bail!(
            "bundle event object {} is {} serialized bytes (max {})",
            event_hash,
            object_bytes,
            MAX_BUNDLE_EVENT_SERIALIZED_BYTES
        );
    }
    let event: BundleEventObject = serde_json::from_value(value)
        .with_context(|| format!("failed to parse bundle event {}", event_hash))?;
    event.validate()?;
    let actual_hash = hash_bundle_event(&event)
        .context("failed to canonicalize bundle event while verifying its hash")?;
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

fn compute_request_fingerprint(
    bundle_id: &str,
    request: &BundleEventAppendRequest,
) -> anyhow::Result<String> {
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
        "attachments": request.attachments,
    });
    let canonical = lillux::canonical_json(&value)
        .context("failed to canonicalize bundle event request fingerprint")?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

fn validate_payload_size(payload: &serde_json::Value) -> anyhow::Result<()> {
    let bytes = lillux::canonical_json(payload)
        .context("failed to canonicalize bundle event payload")?
        .len();
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
            attachments: vec![],
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
    fn event_attachment_is_part_of_the_retained_object_closure() {
        let (_tmp, cas_root, refs_root) = roots();
        let signer = TestSigner::default();
        let trust = trust_store(&signer);
        let checkpoint = b"durable learner checkpoint";
        let blob_hash = lillux::CasStore::new(cas_root.clone())
            .put_blob(checkpoint)
            .unwrap()
            .hash;
        let mut request = append_request("actor", "weights_updated");
        request.attachments.push(BundleEventAttachment {
            name: "checkpoint".to_string(),
            blob_hash: blob_hash.clone(),
            size_bytes: checkpoint.len() as u64,
            media_type: Some("application/octet-stream".to_string()),
        });

        let appended =
            append_bundle_event(&cas_root, &refs_root, request, &signer, &trust).unwrap();
        let closure =
            crate::object_closure::collect_object_closure(&cas_root, [appended.event_hash])
                .unwrap();

        assert!(closure.is_complete());
        assert!(closure.blob_hashes.contains(&blob_hash));
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
        let malformed_json = lillux::canonical_json(&malformed.to_value()).unwrap();
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

    #[test]
    fn bundle_event_cursor_is_signed_and_identity_bound() {
        let signer = TestSigner::default();
        let trust = trust_store(&signer);
        let cursor = BundleEventCursor::new(
            "ryeos-email",
            "email_event",
            "email_1",
            &"a".repeat(64),
            &"b".repeat(64),
            &signer,
            &trust,
        )
        .unwrap();

        cursor.verify("ryeos-email", "email_event", &trust).unwrap();
        assert!(cursor
            .verify("other-bundle", "email_event", &trust)
            .is_err());

        let mut forged = cursor;
        forged.event_hash = "c".repeat(64);
        let error = forged
            .verify("ryeos-email", "email_event", &trust)
            .unwrap_err();
        assert!(format!("{error:#}").contains("signature verification failed"));
    }

    #[test]
    fn bundle_event_cursor_rejects_an_advanced_head() {
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
        let mut second_request = append_request("email_1", "email_approved");
        second_request.expected_chain_head_hash = Some(first.event_hash);
        let second =
            append_bundle_event(&cas_root, &refs_root, second_request, &signer, &trust).unwrap();
        let (_runtime, cas, refs_directory) =
            pin_bundle_event_authority(&cas_root, &refs_root).unwrap();
        let cursor = read_bundle_event_chain_page(
            &cas,
            &refs_directory,
            &trust,
            "ryeos-email",
            "email_event",
            "email_1",
            None,
            1,
            usize::MAX,
            &signer,
        )
        .unwrap()
        .next_cursor
        .expect("two-event chain must yield a cursor");

        let mut third_request = append_request("email_1", "email_sent");
        third_request.expected_chain_head_hash = Some(second.event_hash);
        append_bundle_event(&cas_root, &refs_root, third_request, &signer, &trust).unwrap();

        let error = read_bundle_event_chain_page(
            &cas,
            &refs_directory,
            &trust,
            "ryeos-email",
            "email_event",
            "email_1",
            Some(&cursor),
            1,
            usize::MAX,
            &signer,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("stale bundle event cursor"));
    }
}
