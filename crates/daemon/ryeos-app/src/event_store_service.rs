use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use ryeos_node_runtime::node_policy::sections::event_storage::EventStoragePolicy;
use ryeos_node_runtime::node_policy::sections::thread_attachments::ThreadAttachmentsPolicy;
use ryeos_runtime::{
    CognitionInAssembler, MAX_RUNTIME_EVENT_BATCH_BYTES, MAX_RUNTIME_EVENT_BATCH_ITEMS,
    MAX_RUNTIME_EVENT_PAYLOAD_BYTES,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state_store::{NewEventRecord, NewThreadAttachment, PersistedEventRecord, StateStore};

/// Most append receipts kept to answer a retried idempotent append.
const MAX_APPEND_RECEIPTS: usize = 256;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
/// Most proven (chain, attachment) pairs kept; the cache is cleared at the
/// bound, which costs one more scan.
const MAX_NAMED_ATTACHMENT_CACHE: usize = 4096;
/// The event types that name the attachments a run reads back: tool results
/// that carry images, and inputs.
const ATTACHMENT_NAMING_EVENT_TYPES: [&str; 2] = ["tool_call_result", "cognition_in"];

/// An attachment upload the node's `thread_attachments` policy refuses: too
/// many in one append, a media type it does not accept, or too large. A
/// caller can tell it from a failure of the append itself and drop the
/// attachment instead of giving up.
#[derive(Debug)]
pub struct AttachmentRefusal(pub String);

impl std::fmt::Display for AttachmentRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AttachmentRefusal {}

impl AttachmentRefusal {
    pub fn code(&self) -> &'static str {
        ryeos_runtime::callback::ATTACHMENT_REFUSED_CODE
    }
}

/// The media type and bytes of one stored attachment.
#[derive(Debug, Clone)]
pub struct StoredAttachment {
    pub media_type: String,
    pub bytes: Vec<u8>,
}

fn events_name_attachment(events: &[PersistedEventRecord], attachment: &str) -> Result<bool> {
    for event in events {
        if ryeos_state::objects::payload_attachment_refs(&event.payload)?.contains(&attachment) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What a keyed append returned, kept so that a retry after a lost
/// acknowledgement gets the same answer and writes nothing twice.
#[derive(Debug)]
struct AppendReceipt {
    thread_id: String,
    key: String,
    fingerprint: String,
    persisted: Vec<PersistedEventRecord>,
}

#[derive(Debug, Clone)]
pub struct EventStoreService {
    state_store: Arc<StateStore>,
    receipts: Arc<Mutex<VecDeque<AppendReceipt>>>,
    /// `(chain root, attachment object hash)` pairs already proven to be named
    /// by an event of that chain, so a later read of the same attachment skips
    /// the scan.
    named_attachments: Arc<Mutex<BTreeSet<(String, String)>>>,
}

/// One runtime-appended event. It carries no storage class: the daemon
/// decides it from the node's signed `event_storage` policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventAppendItem {
    pub event_type: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventAppendParams {
    pub thread_id: String,
    pub event: EventAppendItem,
}

/// Bytes that travel inline with the append that names them. The daemon
/// stores them as `thread_attachment` objects under the append's write permit;
/// an event refers to one through the hash of its object in `attachment_refs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventAttachmentUpload {
    pub media_type: String,
    pub bytes_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventAppendBatchParams {
    pub thread_id: String,
    pub events: Vec<EventAppendItem>,
    /// Attachments stored with this append; every one must be named by an
    /// event of the batch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<EventAttachmentUpload>,
    /// A caller-chosen key for the batch. Repeating a key with the same
    /// batch returns the first answer without appending again; repeating it
    /// with a different batch is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventAppendBatchResult {
    pub persisted: Vec<PersistedEventRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventReplayParams {
    #[serde(default)]
    pub chain_root_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub after_chain_seq: Option<i64>,
    #[serde(default = "default_replay_limit")]
    pub limit: usize,
    /// Return only events of these types. `None` returns every type. Each
    /// name must be a known event type; the limit and the byte budget count
    /// only the events that match.
    #[serde(default)]
    pub event_types: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventReplayResult {
    pub events: Vec<PersistedEventRecord>,
    pub next_cursor: Option<i64>,
}

const DEFAULT_REPLAY_LIMIT: usize = 32;
// Shared daemon consumers include the public 200-record replay surface and a
// few bounded internal 500-record scans. Runtime callbacks apply their tighter
// trust-boundary limit before entering this service.
const MAX_REPLAY_LIMIT: usize = 500;
const MAX_REPLAY_SERIALIZED_BYTES: usize = 6 * 1024 * 1024;

fn default_replay_limit() -> usize {
    DEFAULT_REPLAY_LIMIT
}

impl EventStoreService {
    pub fn new(state_store: Arc<StateStore>) -> Self {
        Self {
            state_store,
            receipts: Arc::new(Mutex::new(VecDeque::new())),
            named_attachments: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// One stored attachment, for a caller that holds a thread of the chain
    /// that names it. Meaning-blind: it returns the stored media type and
    /// bytes of any attachment. The hash must be named by an event of the
    /// thread's chain, so a caller cannot read another chain's attachment by
    /// guessing its hash.
    pub fn read_attachment(&self, thread_id: &str, attachment: &str) -> Result<StoredAttachment> {
        ryeos_state::objects::thread_snapshot::validate_canonical_hash(
            "attachment hash",
            attachment,
        )?;
        let chain_root_id = self
            .state_store
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread not found: {thread_id}"))?
            .chain_root_id;
        let key = (chain_root_id.clone(), attachment.to_owned());
        let known = self
            .named_attachments
            .lock()
            .map_err(|_| anyhow::anyhow!("attachment name cache is poisoned"))?
            .contains(&key);
        if !known {
            if !self.chain_names_attachment(&chain_root_id, attachment)? {
                bail!("attachment {attachment} is not named by any event of this chain");
            }
            let mut cache = self
                .named_attachments
                .lock()
                .map_err(|_| anyhow::anyhow!("attachment name cache is poisoned"))?;
            if cache.len() >= MAX_NAMED_ATTACHMENT_CACHE {
                cache.clear();
            }
            cache.insert(key);
        }
        let (media_type, bytes) = self
            .state_store
            .read_thread_attachment(attachment)?
            .ok_or_else(|| anyhow::anyhow!("attachment {attachment} is not stored"))?;
        Ok(StoredAttachment { media_type, bytes })
    }

    /// Whether any indexed event of the chain that can name attachments lists
    /// `attachment` in its `attachment_refs`.
    fn chain_names_attachment(&self, chain_root_id: &str, attachment: &str) -> Result<bool> {
        let types: Vec<String> = ATTACHMENT_NAMING_EVENT_TYPES
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        let mut after = None;
        loop {
            let page = self.state_store.replay_events_of_types(
                chain_root_id,
                None,
                after,
                MAX_REPLAY_LIMIT,
                MAX_REPLAY_SERIALIZED_BYTES,
                Some(&types),
            )?;
            if events_name_attachment(&page.events, attachment)? {
                return Ok(true);
            }
            match (page.has_more, page.events.last()) {
                (true, Some(last)) => after = Some(last.chain_seq),
                _ => return Ok(false),
            }
        }
    }

    /// Append one runtime event, stored under `storage`'s class for it.
    pub fn append(
        &self,
        params: &EventAppendParams,
        storage: &EventStoragePolicy,
        attachments: &ThreadAttachmentsPolicy,
    ) -> Result<PersistedEventRecord> {
        let result = self.append_batch(
            &EventAppendBatchParams {
                thread_id: params.thread_id.clone(),
                events: vec![params.event.clone()],
                attachments: Vec::new(),
                idempotency_key: None,
            },
            storage,
            attachments,
        )?;
        result
            .persisted
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("append returned no persisted event"))
    }

    /// Append a runtime event batch. Each event is stored under the class the
    /// node's signed `event_storage` policy gives it.
    #[tracing::instrument(
        level = "debug",
        name = "event:append_batch",
        skip(self, params, storage),
        fields(
            thread_id = %params.thread_id,
            count = params.events.len(),
        )
    )]
    pub fn append_batch(
        &self,
        params: &EventAppendBatchParams,
        storage: &EventStoragePolicy,
        attachments: &ThreadAttachmentsPolicy,
    ) -> Result<EventAppendBatchResult> {
        let Some(key) = params.idempotency_key.as_deref() else {
            return self.append_batch_once(params, storage, attachments);
        };
        validate_idempotency_key(key)?;
        let fingerprint = batch_fingerprint(params)?;
        // Held across the append so a concurrent retry of the same key waits
        // for the first attempt's answer instead of writing a second batch.
        let mut receipts = self
            .receipts
            .lock()
            .map_err(|_| anyhow::anyhow!("append receipt registry is poisoned"))?;
        if let Some(receipt) = receipts
            .iter()
            .find(|receipt| receipt.thread_id == params.thread_id && receipt.key == key)
        {
            if receipt.fingerprint != fingerprint {
                bail!(
                    "idempotency key '{key}' was already used for a different batch on thread '{}'",
                    params.thread_id
                );
            }
            return Ok(EventAppendBatchResult {
                persisted: receipt.persisted.clone(),
            });
        }
        let result = self.append_batch_once(params, storage, attachments)?;
        if receipts.len() == MAX_APPEND_RECEIPTS {
            receipts.pop_front();
        }
        receipts.push_back(AppendReceipt {
            thread_id: params.thread_id.clone(),
            key: key.to_owned(),
            fingerprint,
            persisted: result.persisted.clone(),
        });
        Ok(result)
    }

    fn append_batch_once(
        &self,
        params: &EventAppendBatchParams,
        storage: &EventStoragePolicy,
        attachments_policy: &ThreadAttachmentsPolicy,
    ) -> Result<EventAppendBatchResult> {
        validate_runtime_event_batch(&params.events)?;
        let uploads = validate_attachment_uploads(params, attachments_policy)?;
        let thread = self
            .state_store
            .get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread not found: {}", params.thread_id))?;

        // Runtime-authored events belong only to a live run. Reject every event
        // on a terminal row, not just a second terminal event: otherwise a
        // self-finalized process could keep its token and append ordinary braid
        // content after the authoritative terminal snapshot.
        if crate::state_store::is_terminal_status(&thread.status) {
            bail!(
                "event append refused: thread '{}' is already terminal ('{}')",
                thread.thread_id,
                thread.status,
            );
        }

        if params
            .events
            .iter()
            .any(|event| event.event_type == ryeos_state::event_types::THREAD_USAGE)
        {
            let held = self
                .state_store
                .latest_thread_event_by_type(
                    &thread.thread_id,
                    ryeos_state::event_types::THREAD_USAGE,
                )?
                .map(|record| {
                    serde_json::from_value::<ryeos_state::ThreadUsage>(record.payload)
                        .context("thread's held thread_usage settlement is invalid")
                })
                .transpose()?;
            validate_thread_usage_steps(&params.events, held)?;
        }

        let events = params
            .events
            .iter()
            .map(|event| {
                let storage_class = stored_storage_class(event, storage)?;
                Ok(NewEventRecord {
                    event_type: event.event_type.clone(),
                    storage_class: storage_class.as_str().to_owned(),
                    payload: event.payload.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let persisted = self
            .state_store
            .append_events_with_attachments_if_thread_running(
                &thread.chain_root_id,
                &thread.thread_id,
                &events,
                &uploads,
            )?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "event append refused: thread '{}' is no longer running",
                    thread.thread_id
                )
            })?;

        Ok(EventAppendBatchResult { persisted })
    }

    #[tracing::instrument(
        level = "debug",
        name = "event:replay",
        skip(self, params),
        fields(
            thread_id = ?params.thread_id,
            chain_root_id = ?params.chain_root_id,
            limit = params.limit,
        )
    )]
    pub fn replay(&self, params: &EventReplayParams) -> Result<EventReplayResult> {
        if params.limit == 0 || params.limit > MAX_REPLAY_LIMIT {
            bail!(
                "events.replay limit must be between 1 and {}",
                MAX_REPLAY_LIMIT
            );
        }
        let event_types = validate_replay_event_types(params.event_types.as_deref())?;
        let (chain_root_id, thread_id) = match (&params.chain_root_id, &params.thread_id) {
            (Some(chain_root_id), thread_id) => (chain_root_id.clone(), thread_id.clone()),
            (None, Some(thread_id)) => {
                let thread = self
                    .state_store
                    .get_thread(thread_id)?
                    .ok_or_else(|| anyhow::anyhow!("thread not found: {thread_id}"))?;
                (thread.chain_root_id, Some(thread_id.clone()))
            }
            (None, None) => bail!("events.replay requires chain_root_id or thread_id"),
        };

        let page = self.state_store.replay_events_of_types(
            &chain_root_id,
            thread_id.as_deref(),
            params.after_chain_seq,
            params.limit,
            MAX_REPLAY_SERIALIZED_BYTES,
            event_types.as_deref(),
        )?;
        let next_cursor = if page.has_more {
            page.events.last().map(|event| event.chain_seq)
        } else {
            None
        };

        Ok(EventReplayResult {
            events: page.events,
            next_cursor,
        })
    }

    /// The thread a live chain tail should currently follow — owner of the
    /// chain's highest-`chain_seq` event. `None` when the chain has no events.
    pub fn chain_head_thread(&self, chain_root_id: &str) -> Result<Option<String>> {
        self.state_store.chain_head_thread(chain_root_id)
    }
}

/// Most event type names one replay may list.
const MAX_REPLAY_EVENT_TYPES: usize = 64;

/// The names of a replay's type filter, each checked against the event
/// vocabulary. An empty list is refused: it would match nothing, and a caller
/// that wants every type passes none.
fn validate_replay_event_types(event_types: Option<&[String]>) -> Result<Option<Vec<String>>> {
    let Some(event_types) = event_types else {
        return Ok(None);
    };
    if event_types.is_empty() || event_types.len() > MAX_REPLAY_EVENT_TYPES {
        bail!("events.replay event_types must list 1..={MAX_REPLAY_EVENT_TYPES} names");
    }
    let mut names = BTreeSet::new();
    for name in event_types {
        ryeos_runtime::RuntimeEventType::parse(name)
            .with_context(|| format!("events.replay event_types names unknown type '{name}'"))?;
        names.insert(name.clone());
    }
    Ok(Some(names.into_iter().collect()))
}

fn validate_idempotency_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > MAX_IDEMPOTENCY_KEY_BYTES
        || !key.bytes().all(|byte| byte.is_ascii_graphic())
    {
        bail!("idempotency_key must be 1..={MAX_IDEMPOTENCY_KEY_BYTES} visible ASCII bytes");
    }
    Ok(())
}

/// The digest of everything a keyed append writes, so a retry is recognised
/// as the same batch and a reused key with other content is refused.
fn batch_fingerprint(params: &EventAppendBatchParams) -> Result<String> {
    let value = serde_json::json!({
        "events": params.events,
        "attachments": params.attachments,
    });
    let canonical = lillux::canonical_json(&value).context("canonicalize event batch")?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

/// Decode the batch's attachments and hold them to the node's
/// `thread_attachments` limits. Every upload must be named by an event of the
/// batch, so none is stored unreferenced, and only an indexed event may name
/// one, so a stored attachment is always reachable from the signed chain.
fn validate_attachment_uploads(
    params: &EventAppendBatchParams,
    policy: &ThreadAttachmentsPolicy,
) -> Result<Vec<NewThreadAttachment>> {
    if params.attachments.is_empty() {
        return Ok(Vec::new());
    }
    if params.attachments.len() > policy.max_attachments_per_turn as usize {
        return Err(AttachmentRefusal(format!(
            "append carries {} attachments (node limit {} per turn)",
            params.attachments.len(),
            policy.max_attachments_per_turn
        ))
        .into());
    }
    let mut named = BTreeSet::new();
    for event in &params.events {
        for hash in ryeos_state::objects::payload_attachment_refs(&event.payload)? {
            named.insert(hash.to_owned());
        }
    }
    let mut seen = BTreeSet::new();
    let mut uploads = Vec::with_capacity(params.attachments.len());
    for upload in &params.attachments {
        if !policy.accepts(&upload.media_type) {
            return Err(AttachmentRefusal(format!(
                "attachment media type '{}' is not accepted by this node",
                upload.media_type
            ))
            .into());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&upload.bytes_b64)
            .context("attachment bytes_b64 is not valid base64")?;
        if bytes.len() as u64 > policy.max_bytes_per_attachment {
            return Err(AttachmentRefusal(format!(
                "attachment is {} bytes (node limit {})",
                bytes.len(),
                policy.max_bytes_per_attachment
            ))
            .into());
        }
        let object = ryeos_state::objects::ThreadAttachment::for_bytes(&upload.media_type, &bytes)?;
        let hash = object.object_hash()?;
        if !seen.insert(hash.clone()) {
            bail!("attachment {hash} is uploaded twice in one append");
        }
        if !named.contains(&hash) {
            bail!("attachment {hash} is not named by any event of the append");
        }
        uploads.push(NewThreadAttachment {
            media_type: upload.media_type.clone(),
            bytes,
        });
    }
    Ok(uploads)
}

fn validate_runtime_event_batch(events: &[EventAppendItem]) -> Result<()> {
    if events.is_empty() {
        bail!("event batch must not be empty");
    }
    if events.len() > MAX_RUNTIME_EVENT_BATCH_ITEMS {
        bail!(
            "event batch contains {} items (max {})",
            events.len(),
            MAX_RUNTIME_EVENT_BATCH_ITEMS
        );
    }
    let mut batch_bytes = 0usize;
    let mut cognition_in_chunks = CognitionInAssembler::default();
    for event in events {
        refuse_daemon_authored(&event.event_type)?;
        if event.event_type == ryeos_state::event_types::MILESTONE
            && event.payload.get("kind").and_then(Value::as_str) == Some("state_anchor")
        {
            bail!(
                "event append refused: `state_anchor` milestones are daemon-authored through runtime.publish_state_anchor"
            );
        }
        if matches!(
            event.event_type.as_str(),
            ryeos_state::event_types::TOOL_CALL_START | ryeos_state::event_types::TOOL_CALL_RESULT
        ) {
            let operation_id = event
                .payload
                .get("operation_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!("event '{}' requires operation_id", event.event_type)
                })?;
            if !ryeos_runtime::callback::valid_action_operation_id(operation_id) {
                bail!(
                    "event '{}' operation_id must be a canonical lowercase SHA-256 digest",
                    event.event_type
                );
            }
        }
        let payload_bytes = serde_json::to_vec(&event.payload)?.len();
        if payload_bytes > MAX_RUNTIME_EVENT_PAYLOAD_BYTES {
            bail!(
                "event '{}' payload is {} bytes (max {})",
                event.event_type,
                payload_bytes,
                MAX_RUNTIME_EVENT_PAYLOAD_BYTES
            );
        }
        batch_bytes = batch_bytes
            .checked_add(payload_bytes)
            .ok_or_else(|| anyhow::anyhow!("event batch byte count overflow"))?;
        if event.event_type == ryeos_state::event_types::COGNITION_IN
            && (event.payload.get("content_chunk").is_some() || cognition_in_chunks.has_pending())
        {
            cognition_in_chunks.push(&event.payload)?;
        } else if cognition_in_chunks.has_pending() {
            bail!(
                "chunked cognition_in batch was interrupted by event '{}'",
                event.event_type
            );
        }
    }
    cognition_in_chunks.finish()?;
    if batch_bytes > MAX_RUNTIME_EVENT_BATCH_BYTES {
        bail!(
            "event batch payload is {} bytes (max {})",
            batch_bytes,
            MAX_RUNTIME_EVENT_BATCH_BYTES
        );
    }
    Ok(())
}

/// The event types a runtime may not append: each has a daemon writer, and
/// another party reads it as the daemon's testimony. The terminal lifecycle
/// types reach the log only through thread finalization; the rest are written
/// by the daemon's own lifecycle, launch and lineage paths, which append
/// through the state store and never through this runtime-facing service.
/// Types with no writer at all (the continuation and command vocabulary) are
/// listed so a runtime cannot be the first to write one.
///
/// `artifact_published` and `thread_facet_set` stay appendable: a runtime
/// copy of either is display or cohort data that readers re-derive.
pub const DAEMON_AUTHORED_EVENT_TYPES: &[&str] = &[
    // Written by the daemon's evidence, observation and accounting paths.
    ryeos_state::event_types::PROVIDER_ATTEMPT_BUDGET_TRANSITION_V1,
    ryeos_state::event_types::HOOK_OBSERVATION_RECORDED,
    ryeos_state::event_types::HOOK_FAILED,
    ryeos_state::event_types::PROJECT_OBSERVATION_RECORDED,
    ryeos_state::event_types::PROVIDER_CALL_OBSERVATION_RECORDED,
    ryeos_state::event_types::ATTENTION_ACKNOWLEDGED,
    ryeos_state::event_types::LAUNCH_BINDING_REBOUND,
    // Birth and start: written by the daemon when it creates and starts a
    // thread. A runtime-written one would let a thread restate the chain's
    // usage subject.
    ryeos_state::event_types::THREAD_CREATED,
    ryeos_state::event_types::THREAD_STARTED,
    // Terminal lifecycle: written only when the daemon finalizes a thread.
    ryeos_state::event_types::THREAD_COMPLETED,
    ryeos_state::event_types::THREAD_FAILED,
    ryeos_state::event_types::THREAD_KILLED,
    ryeos_state::event_types::THREAD_CANCELLED,
    ryeos_state::event_types::THREAD_TIMED_OUT,
    // Lineage and continuation edges the daemon authors.
    ryeos_state::event_types::THREAD_CONTINUED,
    ryeos_state::event_types::CHILD_THREAD_SPAWNED,
    ryeos_state::event_types::CHILD_THREAD_SETTLED,
    ryeos_state::event_types::WORKER_CONTINUATION_ADMITTED,
    ryeos_state::event_types::EDGE_RECORDED,
    // Launch facts the daemon records for the explain view.
    ryeos_state::event_types::AS_LAUNCHED_RESOLUTION,
    ryeos_state::event_types::AS_LAUNCHED_REF_BINDINGS,
    ryeos_state::event_types::RUNTIME_LAUNCH_FACTS,
    ryeos_state::event_types::LAUNCH_AUGMENTATION_CACHE_HIT,
    ryeos_state::event_types::PROCESS_INPUT_ROOT_SELECTED,
    ryeos_state::event_types::LAUNCH_TOOL_READS,
    // Vocabulary with no writer: reserved, not open to a runtime.
    ryeos_state::event_types::CONTINUATION_REQUESTED,
    ryeos_state::event_types::CONTINUATION_ACCEPTED,
    ryeos_state::event_types::COMMAND_SUBMITTED,
    ryeos_state::event_types::COMMAND_CLAIMED,
    ryeos_state::event_types::COMMAND_COMPLETED,
];

/// Refuse an event type a runtime may not append.
fn refuse_daemon_authored(event_type: &str) -> Result<()> {
    if DAEMON_AUTHORED_EVENT_TYPES.contains(&event_type) {
        bail!(
            "event append refused: `{event_type}` is daemon-authored and cannot be appended by a runtime"
        );
    }
    Ok(())
}

/// V5.5 D11: delegate to the typed `RuntimeEventType` enum.
///
/// `crates/engine/ryeos-runtime/src/events.rs` is the single source of truth for
/// the event vocabulary. Both daemon validators and runtime emitters
/// consume the same enum, so adding a variant cannot drift between
/// the two — the producer and consumer surfaces both fail to compile
/// (or accept the new variant) atomically.
fn validate_event_type(event_type: &str) -> Result<()> {
    // Daemon-authored types are refused before the vocabulary check. The
    // budget-transition audit event, for one, is emitted exclusively from
    // committed ledger transitions through the transactional outbox publisher
    // (which appends via the state store, not this runtime-facing service); a
    // runtime-authored copy could poison the outbox exact-once recovery check.
    refuse_daemon_authored(event_type)?;
    ryeos_runtime::RuntimeEventType::parse(event_type).map(|_| ())
}

/// Hold a batch's `thread_usage` events to the cumulative rule: each carries
/// every figure at least as high as the thread's previous settlement. The
/// first settlement of a thread has nothing to be held to. An identical
/// payload is accepted, so a settlement re-sent after a lost acknowledgement
/// is not refused.
fn validate_thread_usage_steps(
    events: &[EventAppendItem],
    held: Option<ryeos_state::ThreadUsage>,
) -> Result<()> {
    let mut previous = held;
    for event in events {
        if event.event_type != ryeos_state::event_types::THREAD_USAGE {
            continue;
        }
        let usage: ryeos_state::ThreadUsage = serde_json::from_value(event.payload.clone())
            .context("event append refused: invalid thread_usage payload")?;
        usage
            .validate()
            .context("event append refused: invalid thread_usage payload")?;
        if let Some(previous) = previous.as_ref()
            && let Some(field) = usage.cumulative_regression_from(previous)
        {
            bail!(
                "event append refused: thread_usage is not cumulative; `{field}` is lower than the thread's previous settlement"
            );
        }
        previous = Some(usage);
    }
    Ok(())
}

/// The class one runtime event is stored under: the node's signed
/// `event_storage` policy decides for progressive events, and every other
/// event is indexed (a `cognition_out` streaming fragment excepted, whose
/// turn-complete record is the durable one).
fn stored_storage_class(
    event: &EventAppendItem,
    storage: &EventStoragePolicy,
) -> Result<ryeos_runtime::StorageClass> {
    validate_event_type(&event.event_type)?;
    let event_type = ryeos_runtime::RuntimeEventType::parse(&event.event_type)?;
    Ok(storage.class_for(event_type, &event.payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stored(event_type: &str, payload: Value) -> Result<&'static str> {
        stored_storage_class(
            &EventAppendItem {
                event_type: event_type.to_owned(),
                payload,
            },
            &EventStoragePolicy::as_shipped(),
        )
        .map(ryeos_runtime::StorageClass::as_str)
    }

    fn upload(media_type: &str, bytes: &[u8]) -> (EventAttachmentUpload, String) {
        let hash = ryeos_state::objects::ThreadAttachment::for_bytes(media_type, bytes)
            .unwrap()
            .object_hash()
            .unwrap();
        (
            EventAttachmentUpload {
                media_type: media_type.to_owned(),
                bytes_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
            hash,
        )
    }

    fn batch(
        refs: &[String],
        attachments: Vec<EventAttachmentUpload>,
        key: Option<&str>,
    ) -> EventAppendBatchParams {
        EventAppendBatchParams {
            thread_id: "T-1".to_owned(),
            events: vec![EventAppendItem {
                event_type: "cognition_frame".to_owned(),
                payload: json!({"attachment_refs": refs}),
            }],
            attachments,
            idempotency_key: key.map(str::to_owned),
        }
    }

    fn recorded(event_type: &str, payload: Value) -> PersistedEventRecord {
        PersistedEventRecord {
            event_id: 1,
            event_hash: None,
            chain_root_id: "T-1".to_owned(),
            chain_seq: 1,
            thread_id: "T-1".to_owned(),
            thread_seq: 1,
            event_type: event_type.to_owned(),
            storage_class: "indexed".to_owned(),
            ts: "2026-11-18T00:00:00Z".to_owned(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload,
        }
    }

    #[test]
    fn an_attachment_is_named_only_by_an_event_that_lists_its_hash() {
        let (_, named) = upload("image/png", b"pixels");
        let (_, other) = upload("image/png", b"other pixels");
        let events = vec![
            recorded("tool_call_result", json!({"parts": []})),
            recorded(
                "tool_call_result",
                json!({"attachment_refs": [named.clone()]}),
            ),
        ];
        assert!(events_name_attachment(&events, &named).unwrap());
        assert!(!events_name_attachment(&events, &other).unwrap());
        assert!(!events_name_attachment(&[], &named).unwrap());
        let malformed = vec![recorded(
            "tool_call_result",
            json!({"attachment_refs": "x"}),
        )];
        assert!(events_name_attachment(&malformed, &named).is_err());
    }

    #[test]
    fn uploads_must_be_named_accepted_and_within_the_node_limits() {
        let policy = ThreadAttachmentsPolicy::as_shipped();
        let (first, first_hash) = upload("application/json", b"{}");
        let (second, second_hash) = upload("image/png", b"pixels");

        let ok = batch(
            &[first_hash.clone(), second_hash.clone()],
            vec![first.clone(), second.clone()],
            None,
        );
        let uploads = validate_attachment_uploads(&ok, &policy).unwrap();
        assert_eq!(uploads.len(), 2);
        assert_eq!(uploads[0].bytes, b"{}");

        let unnamed = batch(
            std::slice::from_ref(&first_hash),
            vec![first.clone(), second],
            None,
        );
        assert!(
            format!(
                "{:#}",
                validate_attachment_uploads(&unnamed, &policy).unwrap_err()
            )
            .contains("not named")
        );

        let twice = batch(
            std::slice::from_ref(&first_hash),
            vec![first.clone(), first],
            None,
        );
        assert!(
            format!(
                "{:#}",
                validate_attachment_uploads(&twice, &policy).unwrap_err()
            )
            .contains("twice")
        );

        let (bmp, bmp_hash) = upload("image/bmp", b"x");
        let refused_type = batch(&[bmp_hash], vec![bmp], None);
        assert!(
            format!(
                "{:#}",
                validate_attachment_uploads(&refused_type, &policy).unwrap_err()
            )
            .contains("not accepted")
        );

        let mut tight = policy.clone();
        tight.max_bytes_per_attachment = 1;
        let (big, big_hash) = upload("application/json", b"{}");
        let too_big = batch(&[big_hash], vec![big], None);
        assert!(
            format!(
                "{:#}",
                validate_attachment_uploads(&too_big, &tight).unwrap_err()
            )
            .contains("node limit")
        );

        let mut one_per_turn = policy;
        one_per_turn.max_attachments_per_turn = 1;
        let (a, a_hash) = upload("application/json", b"{}");
        let (b, b_hash) = upload("application/json", b"[]");
        let too_many = batch(&[a_hash, b_hash], vec![a, b], None);
        assert!(
            format!(
                "{:#}",
                validate_attachment_uploads(&too_many, &one_per_turn).unwrap_err()
            )
            .contains("per turn")
        );
    }

    #[test]
    fn replay_type_filters_name_known_events_only() {
        assert_eq!(validate_replay_event_types(None).unwrap(), None);
        let names = vec!["thread_usage".to_owned(), "cognition_in".to_owned()];
        assert_eq!(
            validate_replay_event_types(Some(&names)).unwrap(),
            Some(vec!["cognition_in".to_owned(), "thread_usage".to_owned()])
        );
        for bad in [
            Vec::new(),
            vec!["not_an_event".to_owned()],
            vec!["cognition_in".to_owned(), "".to_owned()],
            (0..65).map(|_| "thread_usage".to_owned()).collect(),
        ] {
            assert!(validate_replay_event_types(Some(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_batch_without_uploads_needs_no_attachment_check() {
        let policy = ThreadAttachmentsPolicy::as_shipped();
        assert!(
            validate_attachment_uploads(&batch(&[], Vec::new(), None), &policy)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn idempotency_keys_are_bounded_visible_ascii() {
        assert!(validate_idempotency_key("launch-1:frame").is_ok());
        for bad in ["", "has space", "caf\u{e9}", &"k".repeat(129)] {
            assert!(validate_idempotency_key(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_fingerprint_tracks_events_and_attachments_but_not_the_key() {
        let (first, first_hash) = upload("application/json", b"{}");
        let base = batch(
            std::slice::from_ref(&first_hash),
            vec![first.clone()],
            Some("a"),
        );
        let same_other_key = batch(std::slice::from_ref(&first_hash), vec![first], Some("b"));
        assert_eq!(
            batch_fingerprint(&base).unwrap(),
            batch_fingerprint(&same_other_key).unwrap()
        );
        let (second, second_hash) = upload("application/json", b"[]");
        let different = batch(&[second_hash], vec![second], Some("a"));
        assert_ne!(
            batch_fingerprint(&base).unwrap(),
            batch_fingerprint(&different).unwrap()
        );
    }

    #[test]
    fn progressive_cognition_out_is_stored_ephemeral() {
        // Shared rule with the runtime: delta / tool_use_partial / complete tool_use.
        for payload in [
            json!({"delta": "hi"}),
            json!({"tool_use_partial": {"id": "x"}}),
            json!({"tool_use": {"id": "x", "name": "f", "arguments": {}}}),
        ] {
            assert_eq!(
                stored("cognition_out", payload.clone()).unwrap(),
                "ephemeral",
                "cognition_out {payload}"
            );
        }
    }

    #[test]
    fn durable_cognition_out_is_stored_indexed() {
        // A turn-complete cognition_out (tool_calls array) or interrupted seal is
        // the durable record.
        for payload in [
            json!({"content": "done", "tool_calls": []}),
            json!({"content": "partial", "interrupted": true}),
        ] {
            assert_eq!(stored("cognition_out", payload).unwrap(), "indexed");
        }
    }

    #[test]
    fn the_policy_decides_progressive_events() {
        for (event_type, class) in [
            ("token_delta", "ephemeral"),
            ("stream_snapshot", "ephemeral"),
            ("cognition_reasoning", "ephemeral"),
            ("observation", "journal_only"),
            ("graph_foreach_iteration", "journal_only"),
        ] {
            assert_eq!(
                stored(event_type, json!({})).unwrap(),
                class,
                "{event_type}"
            );
        }
    }

    #[test]
    fn floor_events_are_stored_indexed() {
        assert_eq!(stored("artifact_published", json!({})).unwrap(), "indexed");
        assert!(stored("not_an_event", json!({})).is_err());
    }

    #[test]
    fn an_event_carrying_a_storage_class_is_refused() {
        // The wire has no storage class; a runtime that still sends one is
        // refused rather than silently ignored.
        let error = serde_json::from_value::<EventAppendItem>(json!({
            "event_type": "observation",
            "storage_class": "ephemeral",
            "payload": {},
        }))
        .unwrap_err();
        assert!(error.to_string().contains("storage_class"), "{error}");
    }

    #[test]
    fn tool_lifecycle_events_require_one_canonical_operation_identity() {
        for event_type in [
            ryeos_state::event_types::TOOL_CALL_START,
            ryeos_state::event_types::TOOL_CALL_RESULT,
        ] {
            let event = |payload| EventAppendItem {
                event_type: event_type.to_string(),
                payload,
            };
            assert!(
                validate_runtime_event_batch(&[event(json!({
                    "operation_id": "a".repeat(64),
                    "tool": "tool:test/echo",
                    "call_id": "call-1"
                }))])
                .is_ok()
            );
            assert!(
                validate_runtime_event_batch(&[event(json!({
                    "tool": "tool:test/echo",
                    "call_id": "call-1"
                }))])
                .unwrap_err()
                .to_string()
                .contains("requires operation_id")
            );
            assert!(
                validate_runtime_event_batch(&[event(json!({
                    "operation_id": "NOT-A-HASH",
                    "tool": "tool:test/echo",
                    "call_id": "call-1"
                }))])
                .unwrap_err()
                .to_string()
                .contains("canonical lowercase")
            );
        }
    }

    #[test]
    fn runtime_cannot_forge_daemon_authored_evidence() {
        for event_type in [
            ryeos_state::event_types::HOOK_OBSERVATION_RECORDED,
            ryeos_state::event_types::HOOK_FAILED,
            ryeos_state::event_types::PROJECT_OBSERVATION_RECORDED,
            ryeos_state::event_types::PROVIDER_CALL_OBSERVATION_RECORDED,
            ryeos_state::event_types::ATTENTION_ACKNOWLEDGED,
            ryeos_state::event_types::LAUNCH_BINDING_REBOUND,
        ] {
            let error = validate_event_type(event_type).unwrap_err();
            assert!(
                error.to_string().contains("daemon-authored"),
                "unexpected refusal for {event_type}: {error:#}"
            );
            assert!(
                ryeos_runtime::RuntimeEventType::parse(event_type).is_ok(),
                "daemon evidence remains part of the indexed event vocabulary"
            );
        }
    }

    /// The types a runtime append refuses because the vocabulary cannot name
    /// them: the daemon projects these itself and no runtime may emit them.
    const UNPARSEABLE_BY_RUNTIME: &[&str] = &[
        "subprocess_output_observed",
        "resource_budget_transition_v1",
        "resource_precontact_budget_transition_v1",
    ];

    /// Every other registered type, which a runtime may append. A new event
    /// type must be placed in one of these lists or in
    /// `DAEMON_AUTHORED_EVENT_TYPES`, so a type with a daemon writer is
    /// classified when it is added.
    const RUNTIME_APPENDABLE: &[&str] = &[
        "stream_opened",
        "token_delta",
        "stream_snapshot",
        "stream_closed",
        "observation",
        "artifact_published",
        "thread_facet_set",
        "cognition_in",
        "cognition_out",
        "cognition_reasoning",
        "cognition_frame",
        "cognition_compaction",
        "tool_call_start",
        "tool_call_result",
        "graph_started",
        "graph_completed",
        "graph_step_started",
        "graph_step_completed",
        "graph_branch_taken",
        "graph_foreach_started",
        "graph_foreach_iteration",
        "graph_follow_suspended",
        "graph_node_retry",
        "provider_retry",
        "cost_untracked",
        "thread_usage",
        "milestone",
    ];

    #[test]
    fn every_registered_event_type_is_classified_exactly_once() {
        let mut classified = BTreeSet::new();
        for list in [
            DAEMON_AUTHORED_EVENT_TYPES,
            UNPARSEABLE_BY_RUNTIME,
            RUNTIME_APPENDABLE,
        ] {
            for name in list {
                assert!(classified.insert(*name), "`{name}` is classified twice");
            }
        }
        let registered: BTreeSet<&str> = ryeos_state::event_types::ALL.iter().copied().collect();
        assert_eq!(
            classified, registered,
            "a registered event type is unclassified, or a list names an unregistered type"
        );
        for name in ryeos_state::event_types::ALL {
            let refused = validate_event_type(name).is_err();
            let expected = !RUNTIME_APPENDABLE.contains(name);
            assert_eq!(refused, expected, "refusal of `{name}`");
            assert_eq!(
                ryeos_runtime::RuntimeEventType::parse(name).is_err(),
                UNPARSEABLE_BY_RUNTIME.contains(name),
                "vocabulary status of `{name}`"
            );
        }
    }

    #[test]
    fn the_refused_set_is_exactly_the_decided_list() {
        let decided: BTreeSet<&str> = [
            "provider_attempt_budget_transition_v1",
            "hook_observation_recorded",
            "hook_failed",
            "project_observation_recorded",
            "provider_call_observation_recorded",
            "attention_acknowledged",
            "launch_binding_rebound",
            "thread_created",
            "thread_started",
            "thread_completed",
            "thread_failed",
            "thread_killed",
            "thread_cancelled",
            "thread_timed_out",
            "thread_continued",
            "child_thread_spawned",
            "child_thread_settled",
            "worker_continuation_admitted",
            "edge_recorded",
            "as_launched_resolution",
            "as_launched_ref_bindings",
            "runtime_launch_facts",
            "launch_augmentation_cache_hit",
            "process_input_root_selected",
            "launch_tool_reads",
            "continuation_requested",
            "continuation_accepted",
            "command_submitted",
            "command_claimed",
            "command_completed",
        ]
        .into_iter()
        .collect();
        let actual: BTreeSet<&str> = DAEMON_AUTHORED_EVENT_TYPES.iter().copied().collect();
        assert_eq!(actual, decided);
        assert_eq!(actual.len(), DAEMON_AUTHORED_EVENT_TYPES.len());
    }

    #[test]
    fn a_runtime_append_of_a_daemon_authored_type_is_refused_typed_and_whole() {
        for event_type in DAEMON_AUTHORED_EVENT_TYPES {
            let error = validate_event_type(event_type).unwrap_err();
            assert!(
                error.to_string().contains("daemon-authored"),
                "unexpected refusal for {event_type}: {error:#}"
            );
            // One refused event among accepted ones refuses the whole batch.
            let error = validate_runtime_event_batch(&[
                EventAppendItem {
                    event_type: "observation".to_owned(),
                    payload: json!({}),
                },
                EventAppendItem {
                    event_type: (*event_type).to_owned(),
                    payload: json!({}),
                },
            ])
            .unwrap_err();
            assert!(
                error.to_string().contains("daemon-authored"),
                "batch carrying {event_type}: {error:#}"
            );
        }
    }

    #[test]
    fn artifact_published_and_thread_facet_set_stay_appendable() {
        for event_type in ["artifact_published", "thread_facet_set"] {
            validate_event_type(event_type).unwrap();
            validate_runtime_event_batch(&[EventAppendItem {
                event_type: event_type.to_owned(),
                payload: json!({}),
            }])
            .unwrap();
        }
    }

    fn usage_item(
        completed_turns: u32,
        input_tokens: u64,
        output_tokens: u64,
        spend_nanos: i64,
    ) -> EventAppendItem {
        EventAppendItem {
            event_type: "thread_usage".to_owned(),
            payload: json!({
                "completed_turns": completed_turns,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "spend_usd": ryeos_accounting::UsdNanos::from_nanos(spend_nanos).unwrap(),
                "spawns_used": 0,
                "started_at": "2026-06-01T00:00:00Z",
                "settled_at": "2026-06-01T00:05:00Z",
                "last_settled_turn_seq": completed_turns,
                "elapsed_ms": 100,
            }),
        }
    }

    fn held_usage(item: &EventAppendItem) -> ryeos_state::ThreadUsage {
        serde_json::from_value(item.payload.clone()).unwrap()
    }

    #[test]
    fn thread_usage_must_be_cumulative_against_the_threads_previous_settlement() {
        let held = usage_item(2, 200, 20, 5_000);

        // The first settlement of a thread has nothing to be held to.
        validate_thread_usage_steps(&[usage_item(1, 100, 10, 1_000)], None).unwrap();
        // The same payload again (a re-send after a lost acknowledgement) and
        // a higher one are accepted.
        validate_thread_usage_steps(std::slice::from_ref(&held), Some(held_usage(&held))).unwrap();
        validate_thread_usage_steps(&[usage_item(3, 300, 30, 9_000)], Some(held_usage(&held)))
            .unwrap();

        // Any figure that goes down is refused.
        for lower in [
            usage_item(1, 200, 20, 5_000),
            usage_item(2, 199, 20, 5_000),
            usage_item(2, 200, 19, 5_000),
            usage_item(2, 200, 20, 4_999),
        ] {
            let error = validate_thread_usage_steps(&[lower], Some(held_usage(&held))).unwrap_err();
            assert!(error.to_string().contains("not cumulative"), "{error:#}");
        }
        // A mixed step (some figures up, one down) is not cumulative.
        assert!(
            validate_thread_usage_steps(&[usage_item(3, 100, 40, 9_000)], Some(held_usage(&held)))
                .is_err()
        );

        // Settlements inside one batch are held to each other.
        assert!(
            validate_thread_usage_steps(
                &[usage_item(3, 300, 30, 9_000), usage_item(2, 200, 20, 5_000)],
                Some(held_usage(&held))
            )
            .is_err()
        );
        assert!(
            validate_thread_usage_steps(
                &[usage_item(1, 100, 10, 1_000), usage_item(2, 200, 20, 5_000)],
                None
            )
            .is_ok()
        );

        // A payload that is not a settlement is refused.
        let malformed = EventAppendItem {
            event_type: "thread_usage".to_owned(),
            payload: json!({"completed_turns": 1}),
        };
        assert!(validate_thread_usage_steps(&[malformed], None).is_err());
    }

    #[test]
    fn runtime_cannot_forge_state_anchor_milestones() {
        let error = validate_runtime_event_batch(&[EventAppendItem {
            event_type: ryeos_state::event_types::MILESTONE.to_string(),
            payload: json!({
                "kind": "state_anchor",
                "payload": {
                    "manifest_ref": format!("cas:{}", "a".repeat(64))
                }
            }),
        }])
        .unwrap_err();
        assert!(error.to_string().contains("daemon-authored"));
    }

    #[test]
    fn runtime_batch_requires_a_complete_atomic_cognition_input() {
        let content = "ARC evidence\n".repeat(24_000);
        let payloads = ryeos_runtime::encode_cognition_in_payloads(
            &ryeos_runtime::CognitionInput::text("launch", content).pinned(),
        )
        .unwrap();
        assert!(payloads.len() > 1);
        let events = payloads
            .iter()
            .cloned()
            .map(|payload| EventAppendItem {
                event_type: "cognition_in".to_string(),
                payload,
            })
            .collect::<Vec<_>>();

        validate_runtime_event_batch(&events).unwrap();
        let error = validate_runtime_event_batch(&events[..events.len() - 1]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("chunked cognition_in ended after")
        );
    }
}
