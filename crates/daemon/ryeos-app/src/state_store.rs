use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use ryeos_state::chain::{ChainLock, SnapshotUpdate};
use ryeos_state::objects::thread_snapshot::ThreadStatus;
use ryeos_state::objects::ThreadSnapshot;
use ryeos_state::objects::ThreadUsage;
use ryeos_state::queries;
use ryeos_state::signer::Signer;
use ryeos_state::StateDb;
use ryeos_state::UsageSubject;

use crate::projection_health::ThreadProjectionHealth;
use crate::runtime_db;
use crate::write_barrier::{WriteBarrier, WritePermit};
pub use runtime_db::{CommandRecord, NewCommandRecord, RuntimeInfo, StopIntent};

mod projection_access;

use projection_access::committed_value;

const MAX_THREAD_ARTIFACT_ITEMS: usize = 512;
const MAX_THREAD_ARTIFACT_TYPE_BYTES: usize = 1024;
const MAX_THREAD_ARTIFACT_METADATA_BYTES: usize = 256 * 1024;
const MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_THREAD_ARTIFACT_RESPONSE_BYTES: usize = 6 * 1024 * 1024;
const MAX_THREAD_FACET_ITEMS: usize = 128;
const MAX_THREAD_FACET_KEY_BYTES: usize = 4 * 1024;
const MAX_THREAD_FACET_VALUE_BYTES: usize = 256 * 1024;
const MAX_THREAD_FACET_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_THREAD_FACET_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_THREAD_LIST_ENRICHMENT_THREADS: usize = 2_000;
const MAX_THREAD_LIST_FACET_ITEMS: usize = 8 * 1024;
const MAX_THREAD_LIST_FACET_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_THREAD_LIST_FACET_RESPONSE_BYTES: usize = 6 * 1024 * 1024;
const MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES: usize = 4 * 1024 * 1024;
/// Exact JSON budget for the response-facing thread result record. The
/// projection content itself is capped by the 512 KiB ThreadEvent ceiling;
/// four MiB also covers worst-case JSON escaping of a malformed stored error
/// converted to a JSON string.
const MAX_THREAD_RESULT_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct PersistedEventRecord {
    pub event_id: i64,
    /// CAS hash of the signed thread event object for durable records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_hash: Option<String>,
    pub chain_root_id: String,
    pub chain_seq: i64,
    pub thread_id: String,
    pub thread_seq: i64,
    pub event_type: String,
    pub storage_class: String,
    pub ts: String,
    /// Hash links into the braid (truth chrome): present on durable rows,
    /// absent on synthetic/ephemeral records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_chain_event_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_thread_event_hash: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct PersistedEventPage {
    pub events: Vec<PersistedEventRecord>,
    pub has_more: bool,
}

pub struct NodeIdentitySigner {
    fingerprint: String,
    signing_key: lillux::crypto::SigningKey,
}

impl NodeIdentitySigner {
    pub fn from_identity(identity: &crate::identity::NodeIdentity) -> Self {
        Self {
            fingerprint: identity.fingerprint().to_string(),
            signing_key: identity.signing_key().clone(),
        }
    }
}

impl Signer for NodeIdentitySigner {
    fn sign(&self, data: &[u8]) -> Vec<u8> {
        use lillux::crypto::Signer as Ed25519Signer;
        self.signing_key.sign(data).to_bytes().to_vec()
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

#[derive(Debug, Clone)]
pub struct NewThreadRecord {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub item_ref: String,
    pub executor_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub upstream_thread_id: Option<String>,
    pub requested_by: Option<String>,
    pub project_root: Option<PathBuf>,
    pub usage_subject: Option<UsageSubject>,
    pub usage_subject_asserted_by: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewEventRecord {
    pub event_type: String,
    pub storage_class: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewArtifactRecord {
    pub artifact_type: String,
    pub uri: String,
    pub content_hash: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FinalizeThreadRecord {
    pub status: String,
    pub outcome_code: Option<String>,
    pub result_json: Option<Value>,
    pub error_json: Option<Value>,
    pub artifacts: Vec<NewArtifactRecord>,
    pub final_cost: Option<ryeos_engine::contracts::FinalCost>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadArtifactRecord {
    pub artifact_id: i64,
    pub artifact_type: String,
    pub uri: String,
    pub content_hash: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadEdgeRecord {
    pub edge_id: i64,
    pub chain_root_id: String,
    pub source_thread_id: String,
    pub target_thread_id: String,
    pub edge_type: String,
    pub created_at: String,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ServiceChainRetirement {
    pub candidate_chains: usize,
    pub retired_chains: usize,
    pub deleted_rows: usize,
}

#[derive(Debug, Serialize)]
pub struct ThreadListItem {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub status: String,
    pub item_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    /// The predecessor this thread continues, if any. Lets the client identify
    /// chain heads from authoritative edges instead of inferring them.
    pub upstream_thread_id: Option<String>,
    /// The continuation successor this thread handed off to, if any. A thread
    /// with no successor is the current chain head — the only place a follow-up
    /// can braid onto. Derived from the `thread_continued` event, terminal
    /// threads only (mirrors `ThreadDetail`).
    pub successor_thread_id: Option<String>,
    pub requested_by: Option<String>,
    pub project_root: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Auxiliary facts for one thread-list page, loaded under one store lock and
/// grouped in memory. Keeps the UI list path from reacquiring the global store
/// mutex and rerunning projection queries for every row.
#[derive(Debug, Default)]
pub struct ThreadListEnrichment {
    pub follow_waiters: Vec<runtime_db::FollowWaiterSummary>,
    pub facets: HashMap<String, Vec<(String, String)>>,
    pub current_graph_nodes: HashMap<String, (String, u32)>,
}

#[derive(Debug, Default)]
pub struct FollowParentListSnapshot {
    pub waiters: Vec<runtime_db::FollowWaiterSummary>,
    pub parents: Vec<ThreadListItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadResultRecord {
    pub outcome_code: Option<String>,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct ThreadDetail {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub status: String,
    pub item_ref: String,
    pub executor_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub upstream_thread_id: Option<String>,
    /// The continuation successor this thread handed off to, if any. Exposed for
    /// every settled status, not only `continued`: an operator follow-up
    /// preserves a `completed`/`failed` predecessor's status, so those expose a
    /// successor too. Derived from the `thread_continued` event; lets a graph
    /// reconciler / client follow a continuation without scraping event
    /// payloads. `None` for a thread that has not been continued.
    pub successor_thread_id: Option<String>,
    pub requested_by: Option<String>,
    pub project_root: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub runtime: RuntimeInfo,
}

/// Result of an idempotent operator continuation create-or-get.
#[derive(Debug)]
pub enum ContinuationOutcome {
    /// A new successor was created with this request's fingerprint persisted on
    /// its edge. The caller should launch it. Carries the persisted events.
    Created(Vec<PersistedEventRecord>),
    /// The source already has a successor whose recorded fingerprint MATCHES this
    /// request — a duplicate submit. The caller returns this id WITHOUT
    /// relaunching (the existing successor is already launching or done).
    Existing { successor_thread_id: String },
    /// The source is already continued by a request with a DIFFERENT fingerprint.
    Conflict { successor_thread_id: String },
}

/// Result of the atomic pre-launch cleanup transition used after child-lineage
/// admission fails. The store only finalizes when the row is still `created`,
/// has no attached process identity, and has no launch claim. Callers therefore
/// never turn a concurrently launching/running child terminal from a stale
/// status read.
#[derive(Debug)]
pub enum FinalizeCreatedUnattachedOutcome {
    Finalized {
        persisted: Vec<PersistedEventRecord>,
        effective: FinalizeThreadRecord,
    },
    AlreadyTerminal,
    NotCurrent {
        status: String,
        process_attached: bool,
        launch_claimed: bool,
    },
}

/// Result of an atomic finalize-if-live transition. The terminal check,
/// shutdown fence, durable-stop dominance, and terminal write all share one
/// StateStore lock, so a callback winner is observed as `AlreadyTerminal`
/// rather than surfacing an invalid-transition race.
#[derive(Debug)]
pub enum FinalizeIfNonterminalOutcome {
    Finalized {
        persisted: Vec<PersistedEventRecord>,
        effective: FinalizeThreadRecord,
    },
    AlreadyTerminal {
        status: String,
    },
    PreservedForShutdown,
}

/// Result of atomically admitting an execution-owner stop against shutdown and
/// lifecycle finalization.
#[derive(Debug)]
pub enum StopIfAdmissionOpenOutcome {
    Requested(RuntimeInfo),
    AlreadyTerminal,
    PreservedForShutdown,
}

struct Inner {
    state_db: StateDb,
    runtime_db: runtime_db::RuntimeDb,
    signer: Arc<dyn Signer>,
    write_barrier: WriteBarrier,
}

pub struct StateStore {
    inner: Mutex<Inner>,
    projection_health: Arc<ThreadProjectionHealth>,
    process_attachment_admission_open: AtomicBool,
}

impl std::fmt::Debug for StateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateStore")
            .field("inner", &"<Mutex<Inner>>")
            .finish()
    }
}

fn build_snapshot(thread: &NewThreadRecord) -> ThreadSnapshot {
    let now = lillux::time::iso8601_now();
    ThreadSnapshot {
        schema: ryeos_state::objects::SCHEMA_VERSION,
        kind: "thread_snapshot".to_string(),
        thread_id: thread.thread_id.clone(),
        chain_root_id: thread.chain_root_id.clone(),
        status: ThreadStatus::Created,
        kind_name: thread.kind.clone(),
        item_ref: thread.item_ref.clone(),
        executor_ref: thread.executor_ref.clone(),
        launch_mode: thread.launch_mode.clone(),
        current_site_id: thread.current_site_id.clone(),
        origin_site_id: thread.origin_site_id.clone(),
        upstream_thread_id: thread.upstream_thread_id.clone(),
        requested_by: thread.requested_by.clone(),
        project_root: thread.project_root.clone(),
        base_project_snapshot_hash: None,
        result_project_snapshot_hash: None,
        created_at: now.clone(),
        updated_at: now,
        started_at: None,
        finished_at: None,
        result: None,
        outcome_code: None,
        error: None,
        budget: None,
        artifacts: vec![],
        facets: Default::default(),
        last_event_hash: None,
        last_chain_seq: 0,
        last_thread_seq: 0,
    }
}

fn convert_events(
    events: &[NewEventRecord],
    chain_root_id: &str,
    thread_id: &str,
) -> Vec<ryeos_state::objects::ThreadEvent> {
    let now = lillux::time::iso8601_now();
    events
        .iter()
        .enumerate()
        .map(|(idx, event)| ryeos_state::objects::ThreadEvent {
            schema: ryeos_state::objects::SCHEMA_VERSION,
            kind: "thread_event".to_string(),
            chain_root_id: chain_root_id.to_string(),
            chain_seq: 0,
            thread_id: thread_id.to_string(),
            thread_seq: (idx + 1) as u64,
            event_type: event.event_type.clone(),
            durability: match event.storage_class.as_str() {
                "indexed" => ryeos_state::objects::EventDurability::Durable,
                "journal" | "journal_only" => ryeos_state::objects::EventDurability::Journal,
                "ephemeral" => ryeos_state::objects::EventDurability::Ephemeral,
                _ => ryeos_state::objects::EventDurability::Durable,
            },
            ts: now.clone(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload: event.payload.clone(),
        })
        .collect()
}

fn persisted_from_append(
    result: &ryeos_state::chain::AppendResult,
    events: &[NewEventRecord],
) -> Vec<PersistedEventRecord> {
    persisted_from_stored_events(&result.events, events)
}

fn persisted_from_add_thread_with_events(
    result: &ryeos_state::chain::AddThreadWithEventsResult,
    events: &[NewEventRecord],
) -> Vec<PersistedEventRecord> {
    persisted_from_stored_events(&result.events, events)
}

fn persisted_from_stored_events(
    stored_events: &[ryeos_state::objects::ThreadEvent],
    events: &[NewEventRecord],
) -> Vec<PersistedEventRecord> {
    stored_events
        .iter()
        .zip(events.iter())
        .map(|(stored, input)| PersistedEventRecord {
            event_id: stored.chain_seq as i64,
            event_hash: Some(thread_event_hash(stored)),
            chain_root_id: stored.chain_root_id.clone(),
            chain_seq: stored.chain_seq as i64,
            thread_id: stored.thread_id.clone(),
            thread_seq: stored.thread_seq as i64,
            event_type: input.event_type.clone(),
            storage_class: input.storage_class.clone(),
            ts: stored.ts.clone(),
            prev_chain_event_hash: stored.prev_chain_event_hash.clone(),
            prev_thread_event_hash: stored.prev_thread_event_hash.clone(),
            payload: input.payload.clone(),
        })
        .collect()
}

fn thread_event_hash(event: &ryeos_state::objects::ThreadEvent) -> String {
    lillux::sha256_hex(lillux::canonical_json(&event.to_value()).as_bytes())
}

fn ephemeral_record(
    chain_root_id: &str,
    thread_id: &str,
    event: &NewEventRecord,
) -> PersistedEventRecord {
    PersistedEventRecord {
        event_id: 0,
        event_hash: None,
        chain_root_id: chain_root_id.to_string(),
        chain_seq: 0,
        thread_id: thread_id.to_string(),
        thread_seq: 0,
        event_type: event.event_type.clone(),
        storage_class: event.storage_class.clone(),
        ts: lillux::time::iso8601_now(),
        prev_chain_event_hash: None,
        prev_thread_event_hash: None,
        payload: event.payload.clone(),
    }
}

fn append_events_locked(
    g: &Inner,
    chain_root_id: &str,
    thread_id: &str,
    events: &[NewEventRecord],
) -> Result<Vec<PersistedEventRecord>> {
    validate_artifact_event_admission(g, thread_id, events)?;
    validate_facet_event_admission(g, thread_id, events)?;
    let mut records: Vec<Option<PersistedEventRecord>> = vec![None; events.len()];
    let mut durable_events = Vec::new();
    let mut durable_thread_events = Vec::new();
    let mut durable_indices = Vec::new();
    let converted_events = convert_events(events, chain_root_id, thread_id);

    for (idx, (event, thread_event)) in events.iter().zip(converted_events).enumerate() {
        // Validate before separating ephemeral records: ephemeral events bypass
        // CAS, but must observe the same complete-event byte ceiling as every
        // durable writer.
        thread_event.validate()?;
        if event.storage_class == "ephemeral" {
            records[idx] = Some(ephemeral_record(chain_root_id, thread_id, event));
        } else {
            durable_indices.push(idx);
            durable_events.push(event.clone());
            durable_thread_events.push(thread_event);
        }
    }

    if !durable_events.is_empty() {
        let result = committed_value(g.state_db.append_events(
            chain_root_id,
            thread_id,
            durable_thread_events,
            vec![],
            g.signer.as_ref(),
        )?);
        for (idx, record) in durable_indices
            .into_iter()
            .zip(persisted_from_append(&result, &durable_events))
        {
            records[idx] = Some(record);
        }
    }

    records
        .into_iter()
        .map(|record| record.ok_or_else(|| anyhow!("append event record missing")))
        .collect()
}

fn has_indexed_collection_events(events: &[NewEventRecord]) -> bool {
    events.iter().any(|event| {
        event.storage_class == "indexed"
            && matches!(
                event.event_type.as_str(),
                ryeos_state::event_types::ARTIFACT_PUBLISHED
                    | ryeos_state::event_types::THREAD_FACET_SET
            )
    })
}

fn validate_artifact_shape(artifact_type: &str, metadata: Option<&Value>) -> Result<usize> {
    if artifact_type.is_empty() || artifact_type.len() > MAX_THREAD_ARTIFACT_TYPE_BYTES {
        bail!("artifact_type must be 1..={MAX_THREAD_ARTIFACT_TYPE_BYTES} UTF-8 bytes");
    }
    let metadata_bytes = metadata
        .map(serde_json::to_vec)
        .transpose()?
        .map_or(0, |bytes| bytes.len());
    if metadata_bytes > MAX_THREAD_ARTIFACT_METADATA_BYTES {
        bail!(
            "artifact metadata is {metadata_bytes} bytes; maximum is {MAX_THREAD_ARTIFACT_METADATA_BYTES}"
        );
    }
    Ok(metadata_bytes)
}

fn validate_new_artifact_shape(artifact_type: &str, metadata: Option<&Value>) -> Result<usize> {
    let null_metadata = Value::Null;
    validate_artifact_shape(artifact_type, Some(metadata.unwrap_or(&null_metadata)))
}

fn ensure_artifact_projection_capacity(
    g: &Inner,
    thread_id: &str,
    additional_items: usize,
    additional_kind_bytes: usize,
    additional_metadata_bytes: usize,
) -> Result<()> {
    let (current_items, current_kind_bytes, current_metadata_bytes) =
        queries::thread_artifact_stats(g.state_db.projection(), thread_id)?;
    let final_items = current_items
        .checked_add(additional_items)
        .ok_or_else(|| anyhow!("thread artifact count overflow"))?;
    let final_metadata_bytes = current_metadata_bytes
        .checked_add(additional_metadata_bytes)
        .ok_or_else(|| anyhow!("thread artifact byte total overflow"))?;
    let final_kind_bytes = current_kind_bytes
        .checked_add(additional_kind_bytes)
        .ok_or_else(|| anyhow!("thread artifact kind byte total overflow"))?;
    if final_items > MAX_THREAD_ARTIFACT_ITEMS {
        bail!(
            "thread {thread_id} would have {final_items} artifacts; maximum is {MAX_THREAD_ARTIFACT_ITEMS}"
        );
    }
    if final_metadata_bytes > MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES {
        bail!(
            "thread {thread_id} artifact metadata would total {final_metadata_bytes} bytes; maximum is {MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES}"
        );
    }
    // JSON escaping can expand an arbitrary UTF-8 kind by at most six bytes
    // per source byte. Metadata is already stored as serialized JSON. Include
    // conservative fixed record overhead so every newly admitted collection is
    // guaranteed to fit the same response ceiling enforced by readers.
    let fixed_record_bytes = final_items
        .checked_mul(160)
        .ok_or_else(|| anyhow!("thread artifact response estimate overflow"))?;
    let estimated_response_bytes = final_kind_bytes
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(final_metadata_bytes))
        .and_then(|bytes| bytes.checked_add(fixed_record_bytes))
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(|| anyhow!("thread artifact response estimate overflow"))?;
    if estimated_response_bytes > MAX_THREAD_ARTIFACT_RESPONSE_BYTES {
        bail!(
            "thread {thread_id} artifacts would exceed the {MAX_THREAD_ARTIFACT_RESPONSE_BYTES}-byte response maximum"
        );
    }
    Ok(())
}

fn validate_artifact_event_admission(
    g: &Inner,
    thread_id: &str,
    events: &[NewEventRecord],
) -> Result<()> {
    let mut additional_items = 0usize;
    let mut additional_kind_bytes = 0usize;
    let mut additional_metadata_bytes = 0usize;
    for event in events.iter().filter(|event| {
        event.storage_class == "indexed"
            && event.event_type == ryeos_state::event_types::ARTIFACT_PUBLISHED
    }) {
        let artifact_type = event
            .payload
            .get("artifact_type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("artifact_published requires string artifact_type"))?;
        let metadata_bytes = validate_artifact_shape(artifact_type, event.payload.get("metadata"))?;
        additional_items = additional_items
            .checked_add(1)
            .ok_or_else(|| anyhow!("artifact batch count overflow"))?;
        additional_kind_bytes = additional_kind_bytes
            .checked_add(artifact_type.len())
            .ok_or_else(|| anyhow!("artifact batch kind byte total overflow"))?;
        additional_metadata_bytes = additional_metadata_bytes
            .checked_add(metadata_bytes)
            .ok_or_else(|| anyhow!("artifact batch byte total overflow"))?;
    }
    if additional_items > 0 {
        ensure_artifact_projection_capacity(
            g,
            thread_id,
            additional_items,
            additional_kind_bytes,
            additional_metadata_bytes,
        )?;
    }
    Ok(())
}

fn validate_facet_event_admission(
    g: &Inner,
    thread_id: &str,
    events: &[NewEventRecord],
) -> Result<()> {
    let mut updates = HashMap::<String, usize>::new();
    for event in events.iter().filter(|event| {
        event.storage_class == "indexed"
            && event.event_type == ryeos_state::event_types::THREAD_FACET_SET
    }) {
        let key = event
            .payload
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("thread_facet_set requires string key"))?;
        let value = event
            .payload
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("thread_facet_set requires string value"))?;
        if key.is_empty() || key.len() > MAX_THREAD_FACET_KEY_BYTES {
            bail!("facet key must be 1..={MAX_THREAD_FACET_KEY_BYTES} UTF-8 bytes");
        }
        if value.len() > MAX_THREAD_FACET_VALUE_BYTES {
            bail!(
                "facet value is {} bytes; maximum is {MAX_THREAD_FACET_VALUE_BYTES}",
                value.len()
            );
        }
        // Multiple updates to one key in a batch project sequentially; only the
        // final value contributes to the durable facet collection.
        updates.insert(key.to_string(), value.len());
    }
    if updates.is_empty() {
        return Ok(());
    }

    let (mut final_items, mut final_content_bytes) =
        queries::thread_facet_stats(g.state_db.projection(), thread_id)?;
    for (key, value_bytes) in updates {
        match queries::thread_facet_value_bytes(g.state_db.projection(), thread_id, &key)? {
            Some(previous_value_bytes) => {
                final_content_bytes = final_content_bytes
                    .checked_sub(previous_value_bytes)
                    .and_then(|bytes| bytes.checked_add(value_bytes))
                    .ok_or_else(|| anyhow!("thread facet byte total overflow"))?;
            }
            None => {
                final_items = final_items
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("thread facet count overflow"))?;
                final_content_bytes = final_content_bytes
                    .checked_add(key.len())
                    .and_then(|bytes| bytes.checked_add(value_bytes))
                    .ok_or_else(|| anyhow!("thread facet byte total overflow"))?;
            }
        }
    }
    ensure_facet_collection_bounds(thread_id, final_items, final_content_bytes)
}

fn ensure_facet_collection_bounds(
    thread_id: &str,
    final_items: usize,
    final_content_bytes: usize,
) -> Result<()> {
    if final_items > MAX_THREAD_FACET_ITEMS {
        bail!(
            "thread {thread_id} would have {final_items} facets; maximum is {MAX_THREAD_FACET_ITEMS}"
        );
    }
    if final_content_bytes > MAX_THREAD_FACET_CONTENT_BYTES {
        bail!(
            "thread {thread_id} facet content would total {final_content_bytes} bytes; maximum is {MAX_THREAD_FACET_CONTENT_BYTES}"
        );
    }
    let fixed_entry_bytes = final_items
        .checked_mul(8)
        .ok_or_else(|| anyhow!("thread facet response estimate overflow"))?;
    let estimated_response_bytes = final_content_bytes
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(fixed_entry_bytes))
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(|| anyhow!("thread facet response estimate overflow"))?;
    if estimated_response_bytes > MAX_THREAD_FACET_RESPONSE_BYTES {
        bail!(
            "thread {thread_id} facets would exceed the {MAX_THREAD_FACET_RESPONSE_BYTES}-byte response maximum"
        );
    }
    Ok(())
}

fn load_bounded_facets_many(g: &Inner, thread_ids: &[String]) -> Result<Vec<queries::FacetRow>> {
    let mut seen = HashSet::new();
    let thread_ids = thread_ids
        .iter()
        .filter(|thread_id| seen.insert(thread_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if thread_ids.len() > MAX_THREAD_LIST_ENRICHMENT_THREADS {
        bail!(
            "thread-list enrichment requested {} threads; maximum is {MAX_THREAD_LIST_ENRICHMENT_THREADS}",
            thread_ids.len()
        );
    }
    let stats = queries::thread_facet_stats_many(g.state_db.projection(), &thread_ids)?;
    let mut total_items = 0usize;
    let mut total_content_bytes = 0usize;
    for (thread_id, items, content_bytes) in stats {
        ensure_facet_collection_bounds(&thread_id, items, content_bytes)?;
        total_items = total_items
            .checked_add(items)
            .ok_or_else(|| anyhow!("thread-list facet count overflow"))?;
        total_content_bytes = total_content_bytes
            .checked_add(content_bytes)
            .ok_or_else(|| anyhow!("thread-list facet byte total overflow"))?;
    }
    if total_items > MAX_THREAD_LIST_FACET_ITEMS {
        bail!(
            "thread-list facets contain {total_items} entries; maximum is {MAX_THREAD_LIST_FACET_ITEMS}"
        );
    }
    if total_content_bytes > MAX_THREAD_LIST_FACET_CONTENT_BYTES {
        bail!(
            "thread-list facet content is {total_content_bytes} bytes; maximum is {MAX_THREAD_LIST_FACET_CONTENT_BYTES}"
        );
    }
    let fixed_entry_bytes = total_items
        .checked_mul(8)
        .ok_or_else(|| anyhow!("thread-list facet response estimate overflow"))?;
    let estimated_response_bytes = total_content_bytes
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(fixed_entry_bytes))
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(|| anyhow!("thread-list facet response estimate overflow"))?;
    if estimated_response_bytes > MAX_THREAD_LIST_FACET_RESPONSE_BYTES {
        bail!(
            "thread-list facets would exceed the {MAX_THREAD_LIST_FACET_RESPONSE_BYTES}-byte response maximum"
        );
    }
    queries::get_facets_many_bounded(
        g.state_db.projection(),
        &thread_ids,
        MAX_THREAD_LIST_FACET_ITEMS,
        MAX_THREAD_FACET_KEY_BYTES,
        MAX_THREAD_FACET_VALUE_BYTES,
        MAX_THREAD_LIST_FACET_CONTENT_BYTES,
    )
}

/// Which kind of running-source continuation successor to create. Both kinds
/// cut a still-running source, seed the successor's resume context, and settle
/// the source `continued`; they differ only in the edge `reason` recorded and
/// in whether the autonomous chain-depth cap applies. The `GraphFollowResume`
/// marker is daemon-trusted (selectable only via the dedicated method, never a
/// caller-supplied reason).
enum RunningContinuationKind<'a> {
    Machine { sanitized_reason: Option<&'a str> },
    GraphFollowResume,
}

impl StateStore {
    pub fn new(
        runtime_state_dir: PathBuf,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
    ) -> Result<Self> {
        std::fs::create_dir_all(&runtime_state_dir)
            .context("failed to create runtime_state_dir directory")?;

        let projection_health = Arc::new(ThreadProjectionHealth::default());
        let state_db = StateDb::open_with_projection_repair_sink(
            &runtime_state_dir,
            projection_health.clone(),
        )?;
        let runtime_db = runtime_db::RuntimeDb::open(&runtime_db_path)?;
        reconcile_unauthenticated_process_fields(&state_db, &runtime_db)?;

        Ok(Self {
            inner: Mutex::new(Inner {
                state_db,
                runtime_db,
                signer,
                write_barrier,
            }),
            projection_health,
            process_attachment_admission_open: AtomicBool::new(true),
        })
    }

    /// Get the CAS root path for raw CAS access.
    pub fn cas_root(&self) -> Result<std::path::PathBuf> {
        let g = self.lock()?;
        Ok(g.state_db.cas_root().to_path_buf())
    }

    /// Get the refs root path for ref system access.
    pub fn refs_root(&self) -> Result<std::path::PathBuf> {
        let g = self.lock()?;
        Ok(g.state_db.refs_root().to_path_buf())
    }

    /// Run a closure with access to the underlying StateDb.
    pub fn with_state_db<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&StateDb) -> Result<T>,
    {
        let g = self.lock()?;
        f(&g.state_db)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        let started = std::time::Instant::now();
        let guard = self
            .inner
            .lock()
            .map_err(|e| anyhow!("StateStore lock poisoned: {e}"))?;
        let waited = started.elapsed();
        if waited >= std::time::Duration::from_millis(100) {
            tracing::warn!(
                wait_ms = waited.as_millis() as u64,
                "StateStore lock acquisition was delayed"
            );
        }
        Ok(guard)
    }

    fn warn_slow_lock_hold(operation: &'static str, started: std::time::Instant) {
        let held = started.elapsed();
        if held >= std::time::Duration::from_millis(100) {
            tracing::warn!(
                operation,
                hold_ms = held.as_millis() as u64,
                "StateStore lock was held by a slow operation"
            );
        }
    }

    /// Acquire a write permit from the write barrier.
    /// Fails if the daemon is quiescing for GC.
    fn acquire_write_permit(&self) -> Result<WritePermit> {
        let g = self.lock()?;
        g.write_barrier
            .try_acquire()
            .map_err(|e| anyhow!("cannot acquire write permit: {e}"))
    }

    #[tracing::instrument(
        name = "state:create_thread",
        skip(self, thread),
        fields(
            thread_id = %thread.thread_id,
            chain_root_id = %thread.chain_root_id,
            kind = %thread.kind,
            item_ref = %thread.item_ref,
        )
    )]
    pub fn create_thread(&self, thread: &NewThreadRecord) -> Result<Vec<PersistedEventRecord>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("thread creation is closed for daemon shutdown");
        }
        let snapshot = build_snapshot(thread);

        if thread.thread_id == thread.chain_root_id {
            committed_value(g.state_db.create_chain(
                &thread.thread_id,
                snapshot,
                g.signer.as_ref(),
            )?);
        } else {
            committed_value(g.state_db.add_thread(
                &thread.chain_root_id,
                snapshot,
                g.signer.as_ref(),
            )?);
        }

        g.runtime_db
            .insert_thread_runtime(&thread.thread_id, &thread.chain_root_id)?;

        // Edge is derived from snapshot's upstream_thread_id during
        // project_thread_snapshot (see projection.rs). No direct write needed.

        let mut payload = json!({
            "kind": &thread.kind,
            "item_ref": &thread.item_ref,
            "executor_ref": &thread.executor_ref,
            "launch_mode": &thread.launch_mode,
        });
        if let Some(usage_subject) = &thread.usage_subject {
            usage_subject.validate()?;
            payload["usage_subject"] =
                serde_json::to_value(usage_subject).context("failed to encode usage_subject")?;
            if let Some(asserted_by) = &thread.usage_subject_asserted_by {
                payload["usage_subject_asserted_by"] = json!(asserted_by);
            }
        }

        let create_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload,
        };

        let te = convert_events(
            std::slice::from_ref(&create_event),
            &thread.chain_root_id,
            &thread.thread_id,
        );
        let result = committed_value(g.state_db.append_events(
            &thread.chain_root_id,
            &thread.thread_id,
            te,
            vec![],
            g.signer.as_ref(),
        )?);

        Ok(persisted_from_append(&result, &[create_event]))
    }

    #[tracing::instrument(
        name = "state:create_trace_branch",
        skip(self, thread, branch_payload),
        fields(
            thread_id = %thread.thread_id,
            chain_root_id = %thread.chain_root_id,
            item_ref = %thread.item_ref,
        )
    )]
    pub fn create_trace_branch(
        &self,
        thread: &NewThreadRecord,
        branch_payload: Value,
    ) -> Result<Vec<PersistedEventRecord>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("trace-branch creation is closed for daemon shutdown");
        }

        if thread.thread_id == thread.chain_root_id {
            bail!("trace branch child must not be a chain root thread");
        }
        if thread.upstream_thread_id.is_some() {
            bail!("trace branch child must not use upstream_thread_id");
        }
        if g.state_db.get_thread(&thread.thread_id)?.is_some() {
            bail!("thread already exists: {}", thread.thread_id);
        }

        let create_event = NewEventRecord {
            event_type: ryeos_state::event_types::THREAD_CREATED.to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &thread.kind,
                "item_ref": &thread.item_ref,
                "executor_ref": &thread.executor_ref,
                "launch_mode": &thread.launch_mode,
                "trace_branch": true,
            }),
        };
        let branch_event = NewEventRecord {
            event_type: ryeos_state::event_types::EDGE_RECORDED.to_string(),
            storage_class: "indexed".to_string(),
            payload: branch_payload,
        };
        let events_to_append = vec![create_event, branch_event];
        let te = convert_events(&events_to_append, &thread.chain_root_id, &thread.thread_id);
        let result = committed_value(g.state_db.add_thread_with_events(
            &thread.chain_root_id,
            build_snapshot(thread),
            te,
            g.signer.as_ref(),
        )?);

        g.runtime_db
            .insert_thread_runtime(&thread.thread_id, &thread.chain_root_id)?;

        Ok(persisted_from_add_thread_with_events(
            &result,
            &events_to_append,
        ))
    }

    #[tracing::instrument(
        name = "state:mark_thread_running",
        skip(self),
        fields(thread_id = %thread_id)
    )]
    pub fn mark_thread_running(
        &self,
        thread_id: &str,
        base_project_snapshot_hash: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("mark_running is fenced during daemon shutdown");
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing while marking running: {thread_id}"))?;
        if let Some(intent) = runtime.stop_intent {
            bail!(
                "mark_running is fenced after {} request for thread {thread_id}",
                intent.as_str()
            );
        }

        match thread_row.status.as_str() {
            // Fresh launch: fall through to the created -> running transition
            // (appends `thread_started`, sets `started_at`).
            "created" => {}
            // Same-thread crash recovery re-spawns a row that is still `running`,
            // and the resumed runtime calls `mark_running` again. Idempotent
            // no-op: do NOT append a second `thread_started` or rewrite
            // `started_at` — an empty persisted-events list means "already
            // running". (`drain_running_threads` still sees `running`, so the
            // shutdown kill window stays intact — no transient non-running state.)
            "running" => return Ok(Vec::new()),
            other => {
                bail!("invalid status transition: {other} -> running");
            }
        }

        let now = lillux::time::iso8601_now();
        let updated_snapshot = ThreadSnapshot {
            schema: ryeos_state::objects::SCHEMA_VERSION,
            kind: "thread_snapshot".to_string(),
            thread_id: thread_row.thread_id.clone(),
            chain_root_id: thread_row.chain_root_id.clone(),
            status: ThreadStatus::Running,
            kind_name: thread_row.kind.clone(),
            item_ref: thread_row.item_ref.clone(),
            executor_ref: thread_row.executor_ref.clone(),
            launch_mode: thread_row.launch_mode.clone(),
            current_site_id: thread_row.current_site_id.clone(),
            origin_site_id: thread_row.origin_site_id.clone(),
            upstream_thread_id: thread_row.upstream_thread_id.clone(),
            requested_by: thread_row.requested_by.clone(),
            project_root: thread_row.project_root.as_ref().map(PathBuf::from),
            base_project_snapshot_hash: base_project_snapshot_hash.map(String::from),
            result_project_snapshot_hash: None,
            created_at: thread_row.created_at.clone(),
            updated_at: now.clone(),
            started_at: Some(now.clone()),
            finished_at: None,
            result: None,
            outcome_code: None,
            error: None,
            budget: None,
            artifacts: vec![],
            facets: Default::default(),
            last_event_hash: None,
            last_chain_seq: 0,
            last_thread_seq: 0,
        };

        let snapshot_update = SnapshotUpdate {
            thread_id: thread_id.to_string(),
            new_snapshot: updated_snapshot,
        };

        let event = NewEventRecord {
            event_type: "thread_started".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({}),
        };

        let te = convert_events(
            std::slice::from_ref(&event),
            &thread_row.chain_root_id,
            thread_id,
        );
        let result = committed_value(g.state_db.append_events(
            &thread_row.chain_root_id,
            thread_id,
            te,
            vec![snapshot_update],
            g.signer.as_ref(),
        )?);

        Ok(persisted_from_append(&result, &[event]))
    }

    #[tracing::instrument(
        name = "state:finalize_thread",
        skip(self, update),
        fields(thread_id = %thread_id, status = %update.status)
    )]
    pub fn finalize_thread(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.finalize_thread_locked(thread_id, update)
            .map(|(events, _)| events)
    }

    /// Generic lifecycle finalization that also returns the effective record
    /// after global durable-stop dominance. Higher layers must use this form so
    /// scheduler/command/follow side effects match the persisted terminal event.
    pub fn finalize_thread_effective(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        self.finalize_thread_locked(thread_id, update)
    }

    /// Runtime-callback finalization with stop/shutdown policy enforced under
    /// the same StateStore lock as the terminal commit. A durable Cancel/Kill
    /// dominates any self-reported status; shutdown without an explicit stop
    /// rejects the commit so recovery can resume the preserved row.
    pub fn finalize_thread_from_runtime(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        self.finalize_thread_locked(thread_id, update)
    }

    fn finalize_thread_locked(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        self.finalize_thread_with_guard(&g, thread_id, update, false)
    }

    /// Atomically finalize a child-link failure only while the child is still a
    /// never-launched row. This conditional transition is deliberately allowed
    /// after shutdown admission closes: unlike a generic finalizer, the guarded
    /// row has no process for shutdown to own and no launcher entitled to attach
    /// one. A durable stop still dominates the requested failure outcome.
    pub fn finalize_created_unattached_if_current(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<FinalizeCreatedUnattachedOutcome> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;

        if is_terminal_status(&thread_row.status) {
            return Ok(FinalizeCreatedUnattachedOutcome::AlreadyTerminal);
        }

        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        let process_attached =
            runtime.pid.is_some() || runtime.pgid.is_some() || runtime.process_identity.is_some();
        let launch_claimed = g.runtime_db.get_launch_claim(thread_id)?.is_some();
        if thread_row.status != ThreadStatus::Created.as_str() || process_attached || launch_claimed
        {
            return Ok(FinalizeCreatedUnattachedOutcome::NotCurrent {
                status: thread_row.status,
                process_attached,
                launch_claimed,
            });
        }

        let (persisted, effective) =
            self.finalize_thread_with_rows(&g, thread_id, thread_row, runtime, update, true)?;
        Ok(FinalizeCreatedUnattachedOutcome::Finalized {
            persisted,
            effective,
        })
    }

    /// Atomically finalize a nonterminal row, or report the terminal/shutdown
    /// winner without a check-then-write race. A durable Cancel/Kill tombstone
    /// is folded into the effective terminal record by
    /// [`Self::finalize_thread_with_rows`].
    pub fn finalize_if_nonterminal(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<FinalizeIfNonterminalOutcome> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        if is_terminal_status(&thread_row.status) {
            return Ok(FinalizeIfNonterminalOutcome::AlreadyTerminal {
                status: thread_row.status,
            });
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        if runtime.stop_intent.is_none()
            && !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            g.runtime_db.reset_resume_attempts(thread_id)?;
            return Ok(FinalizeIfNonterminalOutcome::PreservedForShutdown);
        }
        let (persisted, effective) =
            self.finalize_thread_with_rows(&g, thread_id, thread_row, runtime, update, false)?;
        Ok(FinalizeIfNonterminalOutcome::Finalized {
            persisted,
            effective,
        })
    }

    fn finalize_thread_with_guard(
        &self,
        g: &Inner,
        thread_id: &str,
        update: &FinalizeThreadRecord,
        allow_closed_admission: bool,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        self.finalize_thread_with_rows(
            g,
            thread_id,
            thread_row,
            runtime,
            update,
            allow_closed_admission,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finalize_thread_with_rows(
        &self,
        g: &Inner,
        thread_id: &str,
        thread_row: queries::ThreadRow,
        runtime: RuntimeInfo,
        update: &FinalizeThreadRecord,
        allow_closed_admission: bool,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        let mut effective_update = update.clone();
        // Stop intent dominates every later finalizer, including administrative
        // failure nets. This is intentionally global: a check-then-finalize
        // caller must not be able to overwrite a stop that committed between
        // its check and this lock acquisition.
        if let Some(intent) = runtime.stop_intent {
            let status = match intent {
                StopIntent::Cancel => "cancelled",
                StopIntent::Kill => "killed",
            };
            effective_update.status = status.to_string();
            effective_update.outcome_code = Some(status.to_string());
            effective_update.result_json = None;
            effective_update.error_json = Some(json!({
                "reason": "durable_stop_intent",
                "intent": intent.as_str(),
            }));
        } else if !allow_closed_admission
            && !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            // Every terminal writer shares the shutdown fence. Otherwise an
            // execution-result fallback could turn a shutdown-owned kill into a
            // terminal failure after drain had taken ownership of the process.
            // A durable stop remains the one exception: it must be allowed to
            // settle to its dominant cancelled/killed outcome while draining.
            bail!("thread finalization is fenced during daemon shutdown");
        }
        let update = &effective_update;

        if is_terminal_status(&thread_row.status) {
            bail!(
                "invalid status transition: {} -> {}",
                thread_row.status,
                update.status
            );
        }

        let now = lillux::time::iso8601_now();
        let terminal_status = ThreadStatus::from_str_lossy(&update.status)
            .ok_or_else(|| anyhow!("invalid terminal status: {}", update.status))?;

        let (additional_artifact_kind_bytes, additional_artifact_metadata_bytes) =
            update.artifacts.iter().try_fold(
                (0usize, 0usize),
                |(kind_total, metadata_total), artifact| {
                    let metadata_bytes = validate_new_artifact_shape(
                        &artifact.artifact_type,
                        artifact.metadata.as_ref(),
                    )?;
                    Ok::<_, anyhow::Error>((
                        kind_total
                            .checked_add(artifact.artifact_type.len())
                            .ok_or_else(|| anyhow!("terminal artifact kind byte total overflow"))?,
                        metadata_total.checked_add(metadata_bytes).ok_or_else(|| {
                            anyhow!("terminal artifact metadata byte total overflow")
                        })?,
                    ))
                },
            )?;
        if !update.artifacts.is_empty() {
            if !self.projection_health.is_current() {
                bail!("artifact admission requires a current thread projection");
            }
            ensure_artifact_projection_capacity(
                g,
                thread_id,
                update.artifacts.len(),
                additional_artifact_kind_bytes,
                additional_artifact_metadata_bytes,
            )?;
        }

        let mut facets = BTreeMap::new();
        if let Some(ref cost) = update.final_cost {
            facets.insert("cost.turns".to_string(), cost.turns.to_string());
            facets.insert(
                "cost.input_tokens".to_string(),
                cost.input_tokens.to_string(),
            );
            facets.insert(
                "cost.output_tokens".to_string(),
                cost.output_tokens.to_string(),
            );
            facets.insert("cost.spend".to_string(), cost.spend.to_string());
            if let Some(ref provider) = cost.provider {
                facets.insert("cost.provider".to_string(), provider.clone());
            }
            // Derived-vs-incurred marker (e.g. a graph's child rollup): kept
            // beside the figures so no reader mistakes a rollup for own-spend.
            if let Some(ref basis) = cost.basis {
                facets.insert("cost.basis".to_string(), basis.clone());
            }
            if let Some(ref metadata) = cost.metadata {
                if let Ok(s) = serde_json::to_string(metadata) {
                    facets.insert("cost.metadata_json".to_string(), s);
                }
            }
        }

        let artifacts_json: Vec<Value> = update
            .artifacts
            .iter()
            .map(|a| serde_json::to_value(a).unwrap())
            .collect();

        let updated_snapshot = ThreadSnapshot {
            schema: ryeos_state::objects::SCHEMA_VERSION,
            kind: "thread_snapshot".to_string(),
            thread_id: thread_row.thread_id.clone(),
            chain_root_id: thread_row.chain_root_id.clone(),
            status: terminal_status,
            kind_name: thread_row.kind.clone(),
            item_ref: thread_row.item_ref.clone(),
            executor_ref: thread_row.executor_ref.clone(),
            launch_mode: thread_row.launch_mode.clone(),
            current_site_id: thread_row.current_site_id.clone(),
            origin_site_id: thread_row.origin_site_id.clone(),
            upstream_thread_id: thread_row.upstream_thread_id.clone(),
            requested_by: thread_row.requested_by.clone(),
            project_root: thread_row.project_root.as_ref().map(PathBuf::from),
            base_project_snapshot_hash: None,
            result_project_snapshot_hash: None,
            created_at: thread_row.created_at.clone(),
            updated_at: now.clone(),
            started_at: thread_row.started_at.clone(),
            finished_at: Some(now.clone()),
            result: update.result_json.clone(),
            outcome_code: update.outcome_code.clone(),
            error: update.error_json.clone(),
            budget: update.final_cost.as_ref().map(|cost| {
                ThreadUsage {
                    completed_turns: cost.turns as u32,
                    input_tokens: cost.input_tokens as u64,
                    output_tokens: cost.output_tokens as u64,
                    spend_usd: cost.spend,
                    spawns_used: 0, // not tracked in FinalCost
                    started_at: thread_row
                        .started_at
                        .clone()
                        .unwrap_or_else(|| thread_row.created_at.clone()),
                    settled_at: now.clone(),
                    last_settled_turn_seq: cost.turns as u64,
                    elapsed_ms: 0, // daemon doesn't track wall-clock time
                    provider_id: None,
                    model: None,
                    profile: None,
                }
            }),
            artifacts: artifacts_json,
            facets,
            last_event_hash: None,
            last_chain_seq: 0,
            last_thread_seq: 0,
        };

        let snapshot_update = SnapshotUpdate {
            thread_id: thread_id.to_string(),
            new_snapshot: updated_snapshot,
        };

        let mut events_to_append = Vec::new();

        for artifact in &update.artifacts {
            events_to_append.push(NewEventRecord {
                event_type: "artifact_published".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "artifact_type": artifact.artifact_type,
                    "uri": artifact.uri,
                    "content_hash": artifact.content_hash,
                    "metadata": artifact.metadata,
                }),
            });
        }

        let mut terminal_payload = json!({
            "outcome_code": update.outcome_code,
            "result": update.result_json,
            "has_error": update.error_json.is_some(),
            "artifact_count": update.artifacts.len(),
        });
        if let Some(err) = &update.error_json {
            if let Some(map) = terminal_payload.as_object_mut() {
                map.insert("error".to_string(), err.clone());
            }
        }
        events_to_append.push(NewEventRecord {
            event_type: terminal_event_type(&update.status)?.to_string(),
            storage_class: "indexed".to_string(),
            payload: terminal_payload,
        });

        let te = convert_events(&events_to_append, &thread_row.chain_root_id, thread_id);
        let result = committed_value(g.state_db.append_events(
            &thread_row.chain_root_id,
            thread_id,
            te,
            vec![snapshot_update],
            g.signer.as_ref(),
        )?);

        Ok((
            persisted_from_append(&result, &events_to_append),
            effective_update,
        ))
    }

    #[tracing::instrument(
        name = "state:create_continuation",
        skip(self, successor),
        fields(
            thread_id = %successor.thread_id,
            chain_root_id = %chain_root_id,
            source_thread_id = %source_thread_id,
        )
    )]
    pub fn create_continuation(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("continuation creation is closed for daemon shutdown");
        }
        let source_row = g
            .state_db
            .get_thread(source_thread_id)?
            .ok_or_else(|| anyhow!("source thread not found: {source_thread_id}"))?;

        if is_terminal_status(&source_row.status)
            && source_row.status != "failed"
            && source_row.status != "completed"
        {
            bail!(
                "cannot continue thread in terminal status '{}'",
                source_row.status
            );
        }

        // Single-successor invariant: a thread is continued at most once. The
        // write permit + lock held here serialize `create_continuation`, so this
        // check-then-create is atomic — a double-submit or race cannot mint
        // sibling successors (which would make `successor_thread_id` ambiguous).
        if let Some(existing) =
            queries::continuation_successor(g.state_db.projection(), source_thread_id)?
        {
            bail!("thread {source_thread_id} already continued as {existing}");
        }

        // Predecessor-immutability contract: an already-terminal source's
        // terminal SNAPSHOT (status + result + outcome) is never rewritten — an
        // operator follow-up onto a completed/failed turn preserves it. The
        // chain still records the handoff as a single append-only
        // `thread_continued` event on the source (the chain log is append-only by
        // nature; the single-successor guard above keeps it to exactly one), plus
        // the successor's `upstream_thread_id` link. "Immutable" therefore means
        // the terminal snapshot/result, not "no further chain events."
        //
        // Settle the source to `continued` only when it is still running — a
        // machine handoff (limit-exhausted) ends the run there. A terminal source
        // is left as-is (rewriting it would erase its result).
        let source_snapshot_updates = if is_terminal_status(&source_row.status) {
            Vec::new()
        } else {
            let now = lillux::time::iso8601_now();
            let source_snapshot = ThreadSnapshot {
                schema: ryeos_state::objects::SCHEMA_VERSION,
                kind: "thread_snapshot".to_string(),
                thread_id: source_row.thread_id.clone(),
                chain_root_id: source_row.chain_root_id.clone(),
                status: ThreadStatus::Continued,
                kind_name: source_row.kind.clone(),
                item_ref: source_row.item_ref.clone(),
                executor_ref: source_row.executor_ref.clone(),
                launch_mode: source_row.launch_mode.clone(),
                current_site_id: source_row.current_site_id.clone(),
                origin_site_id: source_row.origin_site_id.clone(),
                upstream_thread_id: source_row.upstream_thread_id.clone(),
                requested_by: source_row.requested_by.clone(),
                project_root: source_row.project_root.as_ref().map(PathBuf::from),
                base_project_snapshot_hash: None,
                result_project_snapshot_hash: None,
                created_at: source_row.created_at.clone(),
                updated_at: now.clone(),
                started_at: source_row.started_at.clone(),
                finished_at: Some(now),
                result: None,
                outcome_code: Some("continued".to_string()),
                error: None,
                budget: None,
                artifacts: vec![],
                facets: Default::default(),
                last_event_hash: None,
                last_chain_seq: 0,
                last_thread_seq: 0,
            };
            vec![SnapshotUpdate {
                thread_id: source_thread_id.to_string(),
                new_snapshot: source_snapshot,
            }]
        };

        // Ensure successor has upstream_thread_id set to source for edge derivation
        let mut successor_with_upstream = successor.clone();
        if successor_with_upstream.upstream_thread_id.is_none() {
            successor_with_upstream.upstream_thread_id = Some(source_thread_id.to_string());
        }
        let successor_snapshot = build_snapshot(&successor_with_upstream);
        committed_value(g.state_db.add_thread(
            chain_root_id,
            successor_snapshot,
            g.signer.as_ref(),
        )?);

        g.runtime_db
            .insert_thread_runtime(&successor.thread_id, chain_root_id)?;

        // Edge is derived from successor snapshot's upstream_thread_id during
        // projection (see project_thread_snapshot in rye-state). No direct write needed.

        let source_event = NewEventRecord {
            event_type: "thread_continued".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "successor_thread_id": &successor.thread_id,
                "reason": reason,
            }),
        };

        let ste = convert_events(
            std::slice::from_ref(&source_event),
            chain_root_id,
            source_thread_id,
        );
        let source_result = committed_value(g.state_db.append_events(
            chain_root_id,
            source_thread_id,
            ste,
            source_snapshot_updates,
            g.signer.as_ref(),
        )?);

        let successor_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &successor.kind,
                "item_ref": &successor.item_ref,
                "continuation_from": source_thread_id,
            }),
        };

        let sste = convert_events(
            std::slice::from_ref(&successor_event),
            chain_root_id,
            &successor.thread_id,
        );
        let successor_result = committed_value(g.state_db.append_events(
            chain_root_id,
            &successor.thread_id,
            sste,
            vec![],
            g.signer.as_ref(),
        )?);

        let mut all_events = persisted_from_append(&source_result, &[source_event]);
        all_events.extend(persisted_from_append(&successor_result, &[successor_event]));
        Ok(all_events)
    }

    /// Machine continuation handoff (limit cut-off) — the autonomous path.
    ///
    /// Unlike [`Self::create_continuation`] (the operator follow-up, which
    /// accepts a terminal source and leaves it as-is), this enforces the machine
    /// invariants atomically under the write permit + lock:
    ///
    /// - the source must be **exactly `running`** — a cut-off live run. Re-checked
    ///   here, not just by the caller, so a source that goes terminal between the
    ///   caller's check and this commit cannot mint a successor.
    /// - the source must carry a captured `ResumeContext` (spawn-time launch
    ///   identity) — a successor we cannot launch is worse than none.
    /// - the successor's launch metadata is **seeded before the source is settled
    ///   `continued`**, and the runtime-db writes (which can fail independently of
    ///   the source's terminal snapshot) happen first — so any failure aborts the
    ///   handoff with the source still `running` (the runner then fails terminal),
    ///   never `continued` behind an unlaunchable successor.
    pub fn create_machine_continuation(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        // The machine handoff carries a free-form runtime LOG reason. Scrub ALL
        // daemon-reserved markers so a runtime cannot mint an edge the chain-depth
        // walk would treat as an operator reset or a depth-exempt follow.
        let sanitized_reason =
            reason.filter(|r| !queries::ContinuationReasonMarker::is_reserved_str(r));
        self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::Machine { sanitized_reason },
        )
    }

    /// Create the parent's follow-resume successor: a running-source continuation
    /// marked `graph_follow_resume`. Created and seeded only — NOT launched (the
    /// resume path launches it later, once the child's result is available) and
    /// NOT subject to the autonomous chain-depth cap (a follow is structural
    /// progress, not an autonomous run). Daemon-only: the trusted marker cannot be
    /// reached through a runtime-supplied reason.
    pub fn create_follow_resume_successor(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::GraphFollowResume,
        )
    }

    /// Shared core for both running-source continuations (machine handoff and
    /// follow-resume). One atomic op under the write permit + lock: re-verify the
    /// source is running, enforce the single-successor invariant, require the
    /// source's captured ResumeContext, seed the successor (runtime-db writes
    /// first), then settle the source `continued`. A race or seed failure aborts
    /// with the source still running — never `continued` behind an unlaunchable
    /// successor.
    fn create_running_continuation_successor(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        kind: RunningContinuationKind<'_>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let source_row = g
            .state_db
            .get_thread(source_thread_id)?
            .ok_or_else(|| anyhow!("source thread not found: {source_thread_id}"))?;

        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("continuation authoring is closed for daemon shutdown");
        }
        let source_runtime = g
            .runtime_db
            .get_runtime_info(source_thread_id)?
            .ok_or_else(|| anyhow!("source runtime row missing: {source_thread_id}"))?;
        if let Some(intent) = source_runtime.stop_intent {
            bail!(
                "cannot continue stop-requested thread {source_thread_id} ({})",
                intent.as_str()
            );
        }

        // A running-source continuation cuts a still-running source. Re-checked
        // under the lock to close the caller's check-then-commit race; a terminal
        // source is the operator follow-up path, not this one.
        if source_row.status != ThreadStatus::Running.as_str() {
            bail!(
                "running continuation requires a running source; \
                 thread {source_thread_id} is '{}'",
                source_row.status
            );
        }

        // Never braid a successor into the wrong chain.
        if chain_root_id != source_row.chain_root_id {
            bail!(
                "chain_root_id mismatch: requested {chain_root_id}, source \
                 {source_thread_id} is in chain {}",
                source_row.chain_root_id
            );
        }

        // Single-successor invariant (atomic under the held permit + lock).
        if let Some(existing) =
            queries::continuation_successor(g.state_db.projection(), source_thread_id)?
        {
            bail!("thread {source_thread_id} already continued as {existing}");
        }

        // Chain-level ceiling: bound the length of an AUTONOMOUS continuation run.
        // MACHINE handoffs only — a follow-resume edge is structural progress and
        // must be allowed even when the parent chain is already at the cap.
        if let RunningContinuationKind::Machine { .. } = &kind {
            let machine_depth = queries::consecutive_machine_continuation_depth(
                g.state_db.projection(),
                source_thread_id,
                crate::thread_lifecycle::MAX_CONTINUATION_CHAIN_DEPTH,
            )?;
            if machine_depth >= crate::thread_lifecycle::MAX_CONTINUATION_CHAIN_DEPTH {
                bail!(
                    "continuation depth limit reached ({machine_depth}/{}); the autonomous \
                     chain will not continue",
                    crate::thread_lifecycle::MAX_CONTINUATION_CHAIN_DEPTH
                );
            }
        }

        // Require the source's captured launch identity: the successor must be
        // able to fold the chain, or the handoff is pointless.
        let source_resume_context = g
            .runtime_db
            .get_runtime_info(source_thread_id)?
            .and_then(|info| info.launch_metadata)
            .and_then(|m| m.resume_context)
            .ok_or_else(|| {
                anyhow!(
                    "source thread {source_thread_id} has no captured ResumeContext; \
                     cannot create a launchable continuation successor"
                )
            })?;

        // Successor preconditions BEFORE any write: it must belong to the source's
        // chain and, if it names an upstream, name THIS source — never braid a
        // successor into the wrong chain or contradict the edge being created (a
        // later StateDb reject would leave an orphan runtime row behind).
        if successor.chain_root_id != source_row.chain_root_id {
            bail!(
                "successor {} chain_root_id {} does not match source chain {}",
                successor.thread_id,
                successor.chain_root_id,
                source_row.chain_root_id
            );
        }
        match successor.upstream_thread_id.as_deref() {
            None => {}
            Some(id) if id == source_thread_id => {}
            Some(other) => bail!(
                "successor {} declares upstream {other}, not the continuation source {source_thread_id}",
                successor.thread_id
            ),
        }
        let mut successor_with_upstream = successor.clone();
        successor_with_upstream.upstream_thread_id = Some(source_thread_id.to_string());

        // Runtime-db writes FIRST: insert the successor runtime row and seed its
        // launch identity before any state-db successor snapshot or source
        // settle. If the seed fails, only an orphan runtime row exists — no
        // state-db successor edge, source untouched and still running.
        g.runtime_db
            .insert_thread_runtime(&successor.thread_id, chain_root_id)?;
        let successor_meta = crate::launch_metadata::RuntimeLaunchMetadata::default()
            .with_resume_context(source_resume_context);
        g.runtime_db
            .set_launch_metadata(&successor.thread_id, &successor_meta)?;

        // State-db successor snapshot (creates the upstream edge).
        let successor_snapshot = build_snapshot(&successor_with_upstream);
        committed_value(g.state_db.add_thread(
            chain_root_id,
            successor_snapshot,
            g.signer.as_ref(),
        )?);

        // Settle the source to `continued` (running by the check above) in the
        // same append as its `thread_continued` event — the final state change.
        let now = lillux::time::iso8601_now();
        let source_snapshot = ThreadSnapshot {
            schema: ryeos_state::objects::SCHEMA_VERSION,
            kind: "thread_snapshot".to_string(),
            thread_id: source_row.thread_id.clone(),
            chain_root_id: source_row.chain_root_id.clone(),
            status: ThreadStatus::Continued,
            kind_name: source_row.kind.clone(),
            item_ref: source_row.item_ref.clone(),
            executor_ref: source_row.executor_ref.clone(),
            launch_mode: source_row.launch_mode.clone(),
            current_site_id: source_row.current_site_id.clone(),
            origin_site_id: source_row.origin_site_id.clone(),
            upstream_thread_id: source_row.upstream_thread_id.clone(),
            requested_by: source_row.requested_by.clone(),
            project_root: source_row.project_root.as_ref().map(PathBuf::from),
            base_project_snapshot_hash: None,
            result_project_snapshot_hash: None,
            created_at: source_row.created_at.clone(),
            updated_at: now.clone(),
            started_at: source_row.started_at.clone(),
            finished_at: Some(now),
            result: None,
            outcome_code: Some("continued".to_string()),
            error: None,
            budget: None,
            artifacts: vec![],
            facets: Default::default(),
            last_event_hash: None,
            last_chain_seq: 0,
            last_thread_seq: 0,
        };
        let edge_reason: Option<&str> = match &kind {
            RunningContinuationKind::Machine { sanitized_reason } => *sanitized_reason,
            RunningContinuationKind::GraphFollowResume => {
                Some(queries::ContinuationReasonMarker::GraphFollowResume.as_str())
            }
        };
        let source_event = NewEventRecord {
            event_type: "thread_continued".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "successor_thread_id": &successor.thread_id,
                "reason": edge_reason,
            }),
        };
        let ste = convert_events(
            std::slice::from_ref(&source_event),
            chain_root_id,
            source_thread_id,
        );
        let source_result = committed_value(g.state_db.append_events(
            chain_root_id,
            source_thread_id,
            ste,
            vec![SnapshotUpdate {
                thread_id: source_thread_id.to_string(),
                new_snapshot: source_snapshot,
            }],
            g.signer.as_ref(),
        )?);

        let successor_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &successor.kind,
                "item_ref": &successor.item_ref,
                "continuation_from": source_thread_id,
            }),
        };
        let sste = convert_events(
            std::slice::from_ref(&successor_event),
            chain_root_id,
            &successor.thread_id,
        );
        let successor_result = committed_value(g.state_db.append_events(
            chain_root_id,
            &successor.thread_id,
            sste,
            vec![],
            g.signer.as_ref(),
        )?);

        let mut all_events = persisted_from_append(&source_result, &[source_event]);
        all_events.extend(persisted_from_append(&successor_result, &[successor_event]));
        Ok(all_events)
    }

    /// Operator follow-up continuation, made idempotent by a request fingerprint.
    ///
    /// Unlike [`Self::create_continuation`] (whose single-successor guard FAILS on
    /// a second follow-up), this dedups: under the write permit + lock, if the
    /// source already has a successor it compares the recorded
    /// `successor_request_fingerprint` to this request's — a match returns the
    /// existing successor (`Existing`, a double-submit), a mismatch is a
    /// `Conflict`. Otherwise it creates the successor and persists the fingerprint
    /// on the `thread_continued` edge in the SAME critical section, so dedup works
    /// even if the daemon crashes before the runtime emits anything. A terminal
    /// (completed/failed) source keeps its status; a running source is settled
    /// `continued` (same as `create_continuation`).
    pub fn create_or_get_continuation(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
        request_fingerprint: &str,
        resume_context: Option<&crate::launch_metadata::ResumeContext>,
    ) -> Result<ContinuationOutcome> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("continuation creation is closed for daemon shutdown");
        }
        let source_row = g
            .state_db
            .get_thread(source_thread_id)?
            .ok_or_else(|| anyhow!("source thread not found: {source_thread_id}"))?;

        // The caller-supplied chain root must match the source's — a continuation
        // stays in the predecessor's chain. Fail loud on a mismatch rather than
        // braiding a successor into the wrong chain.
        if chain_root_id != source_row.chain_root_id {
            bail!(
                "chain_root_id mismatch: caller passed '{chain_root_id}' but source \
                 '{source_thread_id}' belongs to chain '{}'",
                source_row.chain_root_id
            );
        }

        if is_terminal_status(&source_row.status)
            && source_row.status != "failed"
            && source_row.status != "completed"
        {
            bail!(
                "cannot continue thread in terminal status '{}'",
                source_row.status
            );
        }

        // Idempotent get-or-conflict: a source already continued is deduped by
        // fingerprint instead of failing the single-successor guard.
        if let Some(existing) =
            queries::continuation_successor(g.state_db.projection(), source_thread_id)?
        {
            let existing_fp =
                queries::continuation_fingerprint(g.state_db.projection(), source_thread_id)?;
            return Ok(if existing_fp.as_deref() == Some(request_fingerprint) {
                ContinuationOutcome::Existing {
                    successor_thread_id: existing,
                }
            } else {
                ContinuationOutcome::Conflict {
                    successor_thread_id: existing,
                }
            });
        }

        // No successor yet — create it, persisting the fingerprint on the edge.
        // (Body mirrors `create_continuation`.) A terminal source keeps its
        // snapshot; a running source is settled `continued`.
        let source_snapshot_updates = if is_terminal_status(&source_row.status) {
            Vec::new()
        } else {
            let now = lillux::time::iso8601_now();
            let source_snapshot = ThreadSnapshot {
                schema: ryeos_state::objects::SCHEMA_VERSION,
                kind: "thread_snapshot".to_string(),
                thread_id: source_row.thread_id.clone(),
                chain_root_id: source_row.chain_root_id.clone(),
                status: ThreadStatus::Continued,
                kind_name: source_row.kind.clone(),
                item_ref: source_row.item_ref.clone(),
                executor_ref: source_row.executor_ref.clone(),
                launch_mode: source_row.launch_mode.clone(),
                current_site_id: source_row.current_site_id.clone(),
                origin_site_id: source_row.origin_site_id.clone(),
                upstream_thread_id: source_row.upstream_thread_id.clone(),
                requested_by: source_row.requested_by.clone(),
                project_root: source_row.project_root.as_ref().map(PathBuf::from),
                base_project_snapshot_hash: None,
                result_project_snapshot_hash: None,
                created_at: source_row.created_at.clone(),
                updated_at: now.clone(),
                started_at: source_row.started_at.clone(),
                finished_at: Some(now),
                result: None,
                outcome_code: Some("continued".to_string()),
                error: None,
                budget: None,
                artifacts: vec![],
                facets: Default::default(),
                last_event_hash: None,
                last_chain_seq: 0,
                last_thread_seq: 0,
            };
            vec![SnapshotUpdate {
                thread_id: source_thread_id.to_string(),
                new_snapshot: source_snapshot,
            }]
        };

        let mut successor_with_upstream = successor.clone();
        if successor_with_upstream.upstream_thread_id.is_none() {
            successor_with_upstream.upstream_thread_id = Some(source_thread_id.to_string());
        }
        // Write order mirrors the machine path: runtime row + launch metadata
        // FIRST, then the state-db successor snapshot, then the source edge last.
        // A failure before the edge write leaves at most a runtime row + an
        // unlinked successor snapshot (no authoritative continuation edge), never
        // a `continued`/edge'd source pointing at a half-built successor.
        g.runtime_db
            .insert_thread_runtime(&successor.thread_id, chain_root_id)?;

        // Seed the operator launch context (a `ResumeContext`) on the successor so
        // the row is relaunchable the instant it exists — a crash before the
        // spawned launcher runs leaves a successor the operator can re-drive
        // (idempotently, via the fingerprint) or reconcile can recover, rather
        // than a stranded row with no launch information.
        if let Some(rc) = resume_context {
            let meta = crate::launch_metadata::RuntimeLaunchMetadata::default()
                .with_resume_context(rc.clone());
            g.runtime_db
                .set_launch_metadata(&successor.thread_id, &meta)?;
        }

        let successor_snapshot = build_snapshot(&successor_with_upstream);
        committed_value(g.state_db.add_thread(
            chain_root_id,
            successor_snapshot,
            g.signer.as_ref(),
        )?);

        let source_event = NewEventRecord {
            event_type: "thread_continued".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "successor_thread_id": &successor.thread_id,
                "reason": reason,
                "successor_request_fingerprint": request_fingerprint,
            }),
        };
        let ste = convert_events(
            std::slice::from_ref(&source_event),
            chain_root_id,
            source_thread_id,
        );
        let source_result = committed_value(g.state_db.append_events(
            chain_root_id,
            source_thread_id,
            ste,
            source_snapshot_updates,
            g.signer.as_ref(),
        )?);

        let successor_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &successor.kind,
                "item_ref": &successor.item_ref,
                "continuation_from": source_thread_id,
            }),
        };
        let sste = convert_events(
            std::slice::from_ref(&successor_event),
            chain_root_id,
            &successor.thread_id,
        );
        let successor_result = committed_value(g.state_db.append_events(
            chain_root_id,
            &successor.thread_id,
            sste,
            vec![],
            g.signer.as_ref(),
        )?);

        let mut all_events = persisted_from_append(&source_result, &[source_event]);
        all_events.extend(persisted_from_append(&successor_result, &[successor_event]));
        Ok(ContinuationOutcome::Created(all_events))
    }

    /// The `successor_request_fingerprint` recorded on a source's
    /// `thread_continued` edge, if any — used to dedup operator double-submits.
    pub fn get_continuation_fingerprint(&self, thread_id: &str) -> Result<Option<String>> {
        let g = self.lock()?;
        queries::continuation_fingerprint(g.state_db.projection(), thread_id)
    }

    /// Whether `source_thread_id`'s continuation edge is a follow-resume edge
    /// pointing at `successor_thread_id`. Such a successor has the same shape as a
    /// stranded machine continuation but must NOT be auto-launched — it waits for
    /// the followed child's result. Target-aware: another created row that merely
    /// names the same upstream is NOT matched.
    pub fn is_follow_resume_successor(
        &self,
        source_thread_id: &str,
        successor_thread_id: &str,
    ) -> Result<bool> {
        let g = self.lock()?;
        Ok(matches!(
            queries::continuation_edge(g.state_db.projection(), source_thread_id)?,
            Some((succ, Some(reason), _))
                if succ == successor_thread_id
                    && reason == queries::ContinuationReasonMarker::GraphFollowResume.as_str()
        ))
    }

    pub fn get_thread(&self, thread_id: &str) -> Result<Option<ThreadDetail>> {
        let g = self.lock()?;
        let thread_row = match g.state_db.get_thread(thread_id)? {
            Some(row) => row,
            None => return Ok(None),
        };

        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .unwrap_or_default();

        let successor_thread_id = if is_terminal_status(&thread_row.status) {
            queries::continuation_successor(g.state_db.projection(), thread_id)?
        } else {
            None
        };

        Ok(Some(ThreadDetail {
            thread_id: thread_row.thread_id,
            chain_root_id: thread_row.chain_root_id,
            kind: thread_row.kind,
            status: thread_row.status,
            item_ref: thread_row.item_ref,
            executor_ref: thread_row.executor_ref,
            launch_mode: thread_row.launch_mode,
            current_site_id: thread_row.current_site_id,
            origin_site_id: thread_row.origin_site_id,
            upstream_thread_id: thread_row.upstream_thread_id,
            successor_thread_id,
            requested_by: thread_row.requested_by,
            project_root: thread_row.project_root,
            created_at: thread_row.created_at,
            updated_at: thread_row.updated_at,
            started_at: thread_row.started_at,
            finished_at: thread_row.finished_at,
            runtime,
        }))
    }

    pub fn touch_seat_lease(
        &self,
        thread_id: &str,
        owner: &str,
        surface: &str,
        client_ref: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("seat thread {thread_id} does not exist"))?;
        if thread.kind != "seat_session"
            || thread.status != "running"
            || thread.requested_by.as_deref() != Some(owner)
            || thread.item_ref != surface
        {
            bail!("seat thread {thread_id} is not a matching running owned seat");
        }
        if !g
            .runtime_db
            .touch_seat_lease(thread_id, owner, surface, client_ref)?
        {
            bail!("seat thread {thread_id} is being reaped");
        }
        Ok(())
    }

    pub fn touch_existing_seat_lease(&self, thread_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.touch_existing_seat_lease(thread_id)
    }

    pub fn remove_seat_lease(&self, thread_id: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.remove_seat_lease(thread_id)
    }

    pub fn expired_seat_leases(&self, cutoff_ms: i64) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.expired_seat_leases(cutoff_ms)
    }

    pub fn claim_expired_seat_lease(&self, thread_id: &str, cutoff_ms: i64) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.claim_expired_seat_lease(thread_id, cutoff_ms)
    }

    pub fn retire_service_chains_before(
        &self,
        cutoff: &str,
        dry_run: bool,
    ) -> Result<ServiceChainRetirement> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let candidates = g.state_db.terminal_service_chain_candidates(cutoff)?;
        let mut result = ServiceChainRetirement {
            candidate_chains: candidates.len(),
            ..Default::default()
        };
        for chain_root_id in candidates {
            if !g
                .state_db
                .terminal_service_chain_is_retirable(&chain_root_id, cutoff)?
                || g.runtime_db.chain_has_live_state(&chain_root_id)?
            {
                continue;
            }
            if dry_run {
                result.retired_chains += 1;
                continue;
            }
            // Serialize head removal with every append/import writer, then
            // repeat the terminal/age/runtime checks under that lock. A writer
            // which published a new head before us makes the chain recent; a
            // writer behind us cannot publish until the ref is gone.
            let _chain_lock = ChainLock::acquire(g.state_db.refs_root(), &chain_root_id)?;
            if !g
                .state_db
                .terminal_service_chain_is_retirable(&chain_root_id, cutoff)?
                || g.runtime_db.chain_has_live_state(&chain_root_id)?
            {
                continue;
            }
            // Ref first: a crash after this point leaves no live CAS root. The
            // remaining runtime/projection cleanup is idempotent.
            g.state_db.remove_chain_head_ref(&chain_root_id)?;
            result.deleted_rows += g.runtime_db.delete_chain_runtime(&chain_root_id)?;
            result.deleted_rows += g.state_db.delete_chain_projection(&chain_root_id)?;
            result.retired_chains += 1;
        }
        Ok(result)
    }

    pub fn get_thread_result(&self, thread_id: &str) -> Result<Option<ThreadResultRecord>> {
        let result_row = {
            let g = self.lock()?;
            queries::get_thread_result(g.state_db.projection(), thread_id)?
        };
        let Some(row) = result_row else {
            return Ok(None);
        };
        // JSON parsing and exact response serialization happen after releasing
        // the global store mutex. The query has already bounded both source
        // columns before allocating them.
        let result = match row.result {
            Some(bytes) => Some(serde_json::from_slice::<Value>(&bytes).with_context(|| {
                format!(
                    "malformed JSON in thread_results.result for thread_id {}",
                    thread_id
                )
            })?),
            None => None,
        };
        let record = ThreadResultRecord {
            outcome_code: row.outcome_code,
            result,
            error: row
                .error
                .map(|error| serde_json::from_str::<Value>(&error).unwrap_or(Value::String(error))),
            metadata: None,
        };
        let response_bytes = serde_json::to_vec(&record)?.len();
        if response_bytes > MAX_THREAD_RESULT_RESPONSE_BYTES {
            bail!(
                "thread {thread_id} result response is {response_bytes} bytes; maximum is {MAX_THREAD_RESULT_RESPONSE_BYTES}"
            );
        }
        Ok(Some(record))
    }

    pub fn list_thread_artifacts(&self, thread_id: &str) -> Result<Vec<ThreadArtifactRecord>> {
        let artifact_rows = {
            let g = self.lock()?;
            // Aggregate/count-only preflight happens before the guarded SELECT,
            // so no collection BLOB is copied until the whole collection fits.
            ensure_artifact_projection_capacity(&g, thread_id, 0, 0, 0)?;
            queries::list_thread_artifacts_bounded(
                g.state_db.projection(),
                thread_id,
                MAX_THREAD_ARTIFACT_ITEMS,
                MAX_THREAD_ARTIFACT_TYPE_BYTES,
                MAX_THREAD_ARTIFACT_METADATA_BYTES,
                MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES,
            )?
        };
        let mut records = Vec::with_capacity(artifact_rows.len());
        let mut response_bytes = b"[]".len();
        for (idx, row) in artifact_rows.into_iter().enumerate() {
            let metadata = match row.metadata {
                Some(bytes) => {
                    Some(serde_json::from_slice::<Value>(&bytes).with_context(|| {
                        format!(
                            "malformed JSON in thread_artifacts.metadata \
                             for artifact at index {idx} of thread_id {}",
                            thread_id
                        )
                    })?)
                }
                None => None,
            };
            let record = ThreadArtifactRecord {
                artifact_id: idx as i64 + 1,
                artifact_type: row.kind,
                uri: String::new(),
                content_hash: None,
                metadata,
            };
            let encoded = serde_json::to_vec(&record)?;
            response_bytes = response_bytes
                .checked_add(encoded.len())
                .and_then(|bytes| bytes.checked_add(usize::from(!records.is_empty())))
                .ok_or_else(|| anyhow!("thread artifact response size overflow"))?;
            if response_bytes > MAX_THREAD_ARTIFACT_RESPONSE_BYTES {
                bail!(
                    "thread {thread_id} artifact response exceeds the {MAX_THREAD_ARTIFACT_RESPONSE_BYTES}-byte maximum"
                );
            }
            records.push(record);
        }
        Ok(records)
    }

    #[tracing::instrument(
        name = "state:publish_artifact",
        skip(self, artifact),
        fields(thread_id = %thread_id, artifact_type = %artifact.artifact_type)
    )]
    pub fn publish_artifact(
        &self,
        thread_id: &str,
        artifact: &NewArtifactRecord,
    ) -> Result<(ThreadArtifactRecord, PersistedEventRecord)> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;

        // Artifact projection is derived from the artifact_published event
        // during project_event (see projection.rs). No direct write needed.

        let event = NewEventRecord {
            event_type: "artifact_published".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "artifact_type": artifact.artifact_type,
                "uri": artifact.uri,
                "content_hash": artifact.content_hash,
                "metadata": artifact.metadata,
            }),
        };
        if !self.projection_health.is_current() {
            bail!("artifact admission requires a current thread projection");
        }
        validate_artifact_event_admission(&g, thread_id, std::slice::from_ref(&event))?;

        let te = convert_events(
            std::slice::from_ref(&event),
            &thread_row.chain_root_id,
            thread_id,
        );
        let result = committed_value(g.state_db.append_events(
            &thread_row.chain_root_id,
            thread_id,
            te,
            vec![],
            g.signer.as_ref(),
        )?);

        let persisted = persisted_from_append(&result, &[event]);

        let persisted_event = persisted.into_iter().next().ok_or_else(|| {
            anyhow!("artifact_published event was not persisted for thread {thread_id}")
        })?;

        let artifact_id = persisted_event.event_id;

        let record = ThreadArtifactRecord {
            artifact_id,
            artifact_type: artifact.artifact_type.clone(),
            uri: artifact.uri.clone(),
            content_hash: artifact.content_hash.clone(),
            metadata: artifact.metadata.clone(),
        };

        Ok((record, persisted_event))
    }

    pub fn list_threads(&self, limit: usize) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let rows = queries::list_threads(g.state_db.projection(), limit)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// List threads with optional principal filtering.
    ///
    /// When `filter_principal` is `Some(fp)`, only threads with
    /// `requested_by = fp` are returned. `None` returns all threads
    /// (used by internal callers that intentionally request an unfiltered view).
    pub fn list_threads_filtered(
        &self,
        limit: usize,
        filter_principal: Option<&str>,
    ) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let rows =
                queries::list_threads_filtered(g.state_db.projection(), limit, filter_principal)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// As [`Self::list_threads_filtered`] but with an explicit
    /// [`queries::ThreadSort`] — `Watch` orders active-before-terminal then
    /// newest for the operator dashboard, without changing the default order.
    pub fn list_threads_sorted(
        &self,
        limit: usize,
        filter_principal: Option<&str>,
        sort: queries::ThreadSort,
    ) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let rows = queries::list_threads_sorted(
                g.state_db.projection(),
                limit,
                filter_principal,
                sort,
            )?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// Chain-wide execution usage totals (tokens, cost, turns, thread count)
    /// for a `chain_root_id` — the deep-watch summary of an execution and its
    /// continuations.
    pub fn chain_usage_totals(&self, chain_root_id: &str) -> Result<queries::ThreadUsageTotals> {
        let g = self.lock()?;
        queries::sum_thread_usage_latest_by_chain(g.state_db.projection(), chain_root_id)
    }

    /// Node-wide usage settled since `since_iso` (inclusive), continuation-
    /// deduped — what cognition on this node cost inside the window.
    pub fn node_usage_totals_since(&self, since_iso: &str) -> Result<queries::ThreadUsageTotals> {
        let g = self.lock()?;
        queries::sum_thread_usage_latest_since(g.state_db.projection(), since_iso)
    }

    /// Per-status thread counts for the node pulse: non-terminal statuses
    /// count unconditionally, terminal ones only inside the window.
    pub fn thread_status_counts(&self, since_iso: &str) -> Result<Vec<(String, i64)>> {
        let g = self.lock()?;
        queries::thread_status_counts(g.state_db.projection(), since_iso)
    }

    /// As [`Self::list_threads_sorted`] but with the full optional filter set
    /// (status / kind / requested_by) the operator dashboard narrows by.
    pub fn list_threads_query(
        &self,
        limit: usize,
        filter: &queries::ThreadListFilter,
        sort: queries::ThreadSort,
    ) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let hold_started = std::time::Instant::now();
            let rows = queries::list_threads_query(g.state_db.projection(), limit, filter, sort)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            Self::warn_slow_lock_hold("list_threads_query", hold_started);
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// Project thread rows into `ThreadListItem`s, resolving each terminal
    /// thread's continuation successor so the client can identify chain heads
    /// (a head has no successor). Shared by the filtered and unfiltered list
    /// paths.
    fn continuation_payloads_for_rows(
        g: &Inner,
        rows: &[queries::ThreadRow],
    ) -> Result<HashMap<String, Vec<u8>>> {
        let terminal_thread_ids = rows
            .iter()
            .filter(|row| is_terminal_status(&row.status))
            .map(|row| row.thread_id.clone())
            .collect::<Vec<_>>();
        queries::continuation_successor_payloads(
            g.state_db.projection(),
            &terminal_thread_ids,
            MAX_THREAD_LIST_ENRICHMENT_THREADS,
            MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES,
            MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES,
        )
    }

    fn rows_to_list_items(
        rows: Vec<queries::ThreadRow>,
        successor_payloads: HashMap<String, Vec<u8>>,
    ) -> Result<Vec<ThreadListItem>> {
        let mut successors = HashMap::new();
        for (thread_id, payload) in successor_payloads {
            let value: serde_json::Value = serde_json::from_slice(&payload)
                .context("parse thread_continued payload for thread-list enrichment")?;
            if let Some(successor) = value
                .get("successor_thread_id")
                .and_then(serde_json::Value::as_str)
            {
                successors.insert(thread_id, successor.to_string());
            }
        }
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let successor_thread_id = successors.remove(&row.thread_id);
            items.push(ThreadListItem {
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                created_at: row.created_at,
                updated_at: row.updated_at,
            });
        }
        Ok(items)
    }

    /// Load the non-thread-row facts used to decorate one list page. Facets,
    /// current graph nodes, and live follow waiters share one outer store lock
    /// instead of relocking the store for every row.
    pub fn thread_list_enrichment(&self, thread_ids: &[String]) -> Result<ThreadListEnrichment> {
        let (facet_rows, graph_node_payloads, follow_waiters) = {
            let g = self.lock()?;
            let hold_started = std::time::Instant::now();
            let result = (
                load_bounded_facets_many(&g, thread_ids)?,
                queries::current_graph_node_payloads(
                    g.state_db.projection(),
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES,
                )?,
                g.runtime_db.follow_waiter_summaries_for_threads(
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                )?,
            );
            Self::warn_slow_lock_hold("thread_list_enrichment", hold_started);
            result
        };
        Self::assemble_thread_list_enrichment(facet_rows, graph_node_payloads, follow_waiters)
    }

    pub fn thread_list_enrichment_with_waiters(
        &self,
        thread_ids: &[String],
        follow_waiters: Vec<runtime_db::FollowWaiterSummary>,
    ) -> Result<ThreadListEnrichment> {
        let (facet_rows, graph_node_payloads) = {
            let g = self.lock()?;
            let hold_started = std::time::Instant::now();
            let result = (
                load_bounded_facets_many(&g, thread_ids)?,
                queries::current_graph_node_payloads(
                    g.state_db.projection(),
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES,
                )?,
            );
            Self::warn_slow_lock_hold("thread_list_enrichment_with_waiters", hold_started);
            result
        };
        Self::assemble_thread_list_enrichment(facet_rows, graph_node_payloads, follow_waiters)
    }

    fn assemble_thread_list_enrichment(
        facet_rows: Vec<queries::FacetRow>,
        graph_node_payloads: HashMap<String, Vec<u8>>,
        follow_waiters: Vec<runtime_db::FollowWaiterSummary>,
    ) -> Result<ThreadListEnrichment> {
        let mut facets: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for row in facet_rows {
            let value =
                String::from_utf8(row.value).context("thread-list facet value is not UTF-8")?;
            facets
                .entry(row.thread_id)
                .or_default()
                .push((row.key, value));
        }
        let mut current_graph_nodes = HashMap::new();
        for (thread_id, payload) in graph_node_payloads {
            let payload: serde_json::Value = match serde_json::from_slice(&payload) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(
                        %thread_id,
                        %error,
                        "ignoring malformed graph_step_started payload in thread-list enrichment"
                    );
                    continue;
                }
            };
            let Some(node) = payload.get("node").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let step = payload
                .get("step")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32;
            current_graph_nodes.insert(thread_id, (node.to_string(), step));
        }
        Ok(ThreadListEnrichment {
            follow_waiters,
            facets,
            current_graph_nodes,
        })
    }

    /// One consistent runtime waiter snapshot plus its projected suspended
    /// parents. Parent rows are fetched in bounded batches under the same store
    /// lock, avoiding one lock/query cycle per waiter.
    pub fn follow_parent_list_snapshot(&self) -> Result<FollowParentListSnapshot> {
        let (waiters, rows, successor_payloads) = {
            let g = self.lock()?;
            let hold_started = std::time::Instant::now();
            let waiters = g
                .runtime_db
                .follow_waiter_summaries_bounded(MAX_THREAD_LIST_ENRICHMENT_THREADS)?;
            let mut parent_ids = waiters
                .iter()
                .map(|waiter| waiter.parent_thread_id.clone())
                .collect::<Vec<_>>();
            parent_ids.sort();
            parent_ids.dedup();
            let rows = queries::get_threads_many(g.state_db.projection(), &parent_ids)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            Self::warn_slow_lock_hold("follow_parent_list_snapshot", hold_started);
            (waiters, rows, payloads)
        };
        let parents = Self::rows_to_list_items(rows, successor_payloads)?;
        Ok(FollowParentListSnapshot { waiters, parents })
    }

    pub fn summarize_usage_by_subject(
        &self,
        filter: queries::UsageSummaryFilter<'_>,
    ) -> Result<Vec<queries::UsageSummaryRow>> {
        let g = self.lock()?;
        queries::summarize_usage_by_subject(g.state_db.projection(), filter)
    }

    pub fn list_thread_children(&self, thread_id: &str) -> Result<Vec<ThreadDetail>> {
        let g = self.lock()?;
        let child_rows = queries::list_thread_children(g.state_db.projection(), thread_id)?;
        let mut children = Vec::new();
        for row in child_rows {
            let runtime = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .unwrap_or_default();
            let successor_thread_id = if is_terminal_status(&row.status) {
                queries::continuation_successor(g.state_db.projection(), &row.thread_id)?
            } else {
                None
            };
            children.push(ThreadDetail {
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                executor_ref: row.executor_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                created_at: row.created_at,
                updated_at: row.updated_at,
                started_at: row.started_at,
                finished_at: row.finished_at,

                runtime,
            });
        }
        Ok(children)
    }

    pub fn list_chain_threads(&self, chain_root_id: &str) -> Result<Vec<ThreadDetail>> {
        let g = self.lock()?;
        let thread_rows = queries::list_threads_by_chain(g.state_db.projection(), chain_root_id)?;
        let mut threads = Vec::new();
        for row in thread_rows {
            let runtime = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .unwrap_or_default();
            let successor_thread_id = if is_terminal_status(&row.status) {
                queries::continuation_successor(g.state_db.projection(), &row.thread_id)?
            } else {
                None
            };
            threads.push(ThreadDetail {
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                executor_ref: row.executor_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                created_at: row.created_at,
                updated_at: row.updated_at,
                started_at: row.started_at,
                finished_at: row.finished_at,

                runtime,
            });
        }
        Ok(threads)
    }

    pub fn list_chain_edges(&self, chain_root_id: &str) -> Result<Vec<ThreadEdgeRecord>> {
        let g = self.lock()?;
        let edge_rows = queries::list_thread_edges(g.state_db.projection(), chain_root_id)?;
        Ok(edge_rows
            .into_iter()
            .enumerate()
            .map(|(idx, row)| ThreadEdgeRecord {
                edge_id: idx as i64 + 1,
                chain_root_id: row.chain_root_id,
                source_thread_id: row.parent_thread_id,
                target_thread_id: row.child_thread_id,
                edge_type: "spawned".to_string(),
                created_at: String::new(),
                metadata: row.spawn_reason.map(|r| json!(r)),
            })
            .collect())
    }

    pub fn list_threads_by_status(&self, statuses: &[&str]) -> Result<Vec<ThreadDetail>> {
        let g = self.lock()?;
        let thread_rows = queries::list_threads_by_status(g.state_db.projection(), statuses)?;
        let mut details = Vec::new();
        for row in thread_rows {
            let runtime = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .unwrap_or_default();
            let successor_thread_id = if is_terminal_status(&row.status) {
                queries::continuation_successor(g.state_db.projection(), &row.thread_id)?
            } else {
                None
            };
            details.push(ThreadDetail {
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                executor_ref: row.executor_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                created_at: row.created_at,
                updated_at: row.updated_at,
                started_at: row.started_at,
                finished_at: row.finished_at,

                runtime,
            });
        }
        Ok(details)
    }

    pub fn active_thread_count(&self) -> Result<i64> {
        let g = self.lock()?;
        queries::active_thread_count(g.state_db.projection())
    }

    /// Immutable project snapshots required by active or queued runtimes.
    ///
    /// These runtime-DB references are not signed CAS heads, so online GC must
    /// add them as daemon-authoritative transient roots. Retain both fields
    /// defensively if a record carries both; resume selection intentionally
    /// chooses one, while reachability must not collect either active pin.
    pub fn active_resume_snapshot_roots(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        let statuses = [
            ThreadStatus::Created.as_str(),
            ThreadStatus::Running.as_str(),
        ];
        let rows = queries::list_threads_by_status(g.state_db.projection(), &statuses)?;
        let mut roots = std::collections::BTreeSet::new();
        for row in rows {
            let Some(metadata) = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .and_then(|info| info.launch_metadata)
            else {
                continue;
            };
            let Some(resume) = metadata.resume_context else {
                continue;
            };
            if let Some(hash) = resume.original_snapshot_hash {
                roots.insert(hash);
            }
            if let Some(pushed) = resume.original_pushed_head_ref {
                roots.insert(pushed.snapshot_hash);
            }
        }
        Ok(roots.into_iter().collect())
    }

    /// Read a thread's persisted launch metadata (resume context), if any.
    pub fn get_launch_metadata(
        &self,
        thread_id: &str,
    ) -> Result<Option<crate::launch_metadata::RuntimeLaunchMetadata>> {
        let g = self.lock()?;
        Ok(g.runtime_db
            .get_runtime_info(thread_id)?
            .and_then(|info| info.launch_metadata))
    }

    /// Seed a thread's launch identity (resume context / continuation spec) at
    /// spawn time so a continuation successor can be relaunched later with no
    /// live request. Metadata-only (does not touch pid/pgid); the
    /// clobber-preserving attach keeps it against a later empty self-attach.
    pub fn seed_launch_metadata(
        &self,
        thread_id: &str,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.set_launch_metadata(thread_id, launch_metadata)
    }

    #[tracing::instrument(
        name = "state:attach_thread_process",
        skip(self, launch_metadata),
        fields(thread_id = %thread_id, pid = pid, pgid = pgid)
    )]
    pub fn attach_thread_process(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        process_identity: &crate::process::ExecutionProcessIdentity,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
    ) -> Result<()> {
        let g = self.lock()?;
        // The projection row is the authoritative lifecycle identity. A bare
        // runtime row must never acquire a process that reconcile/drain cannot
        // subsequently account for.
        let thread = g.state_db.get_thread(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("thread not found before process attach: {thread_id}")
        })?;
        let exact_repeat = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .is_some_and(|runtime| {
                runtime.pid == Some(pid)
                    && runtime.pgid == Some(pgid)
                    && runtime.process_identity.as_ref() == Some(process_identity)
            });
        if !exact_repeat
            && !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            anyhow::bail!("process attachment admission is closed for daemon shutdown");
        }
        // Defensive: skip attach if the thread was already finalized
        // (e.g. cancelled while the runner was between spawn and attach). An
        // exact identity repeat remains safe and idempotent: fast callback
        // runtimes can self-attach and finalize before their in-process owner
        // reaches the same attach call.
        if is_terminal_status(&thread.status) && !exact_repeat {
            tracing::warn!(
                thread_id,
                status = %thread.status,
                pid,
                pgid,
                "skipping attach_process — thread already terminal"
            );
            anyhow::bail!(
                "refusing to attach process {pid}/{pgid} to terminal thread {thread_id} ({})",
                thread.status
            );
        }
        if !exact_repeat
            && g.runtime_db
                .launch_window_is_cancelled(&thread.chain_root_id)?
        {
            anyhow::bail!(
                "refusing to attach process {pid}/{pgid} to cancelled launch-window member {thread_id}"
            );
        }
        g.runtime_db
            .attach_process(thread_id, pid, pgid, process_identity, launch_metadata)
    }

    /// Close process attachment admission at the shutdown serialization point.
    /// Taking the StateStore lock first waits for every prior attach to commit;
    /// every later attach acquires the lock and observes the closed gate.
    pub fn close_process_attachment_admission(&self) -> Result<()> {
        let _g = self.lock()?;
        self.process_attachment_admission_open
            .store(false, Ordering::Release);
        Ok(())
    }

    /// Whether this daemon instance still admits/finalizes live executions.
    /// Shutdown closes the gate before draining; process owners that wake from
    /// a shutdown-owned signal use this to preserve resumable rows instead of
    /// misclassifying the interruption as an execution failure.
    pub fn process_attachment_admission_is_open(&self) -> bool {
        self.process_attachment_admission_open
            .load(Ordering::Acquire)
    }

    /// Fail closed before a runtime callback authors or dispatches new work.
    /// Durable child-link/continuation mutations repeat this check or inherit
    /// the stop under the same store lock; this front-door check avoids doing
    /// expensive resolution after the authoring fence is already closed.
    pub fn ensure_running_runtime_mutation_allowed(&self, thread_id: &str) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            anyhow::bail!("runtime authoring is closed for daemon shutdown");
        }
        let runtime = g.runtime_db.get_runtime_info(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("runtime row missing for callback thread {thread_id}")
        })?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("callback thread not found: {thread_id}"))?;
        if thread.status != "running" {
            anyhow::bail!(
                "runtime mutation requires a running thread; {thread_id} is {}",
                thread.status
            );
        }
        if let Some(intent) = runtime.stop_intent {
            anyhow::bail!(
                "runtime mutation is closed for stop-requested thread {thread_id} ({})",
                intent.as_str()
            );
        }
        Ok(())
    }

    /// Atomically admit a non-read runtime callback against authoritative
    /// lifecycle, stop, and shutdown state. This is the callback
    /// linearization point: a request admitted here may finish, while every
    /// request that arrives after terminal/stop/shutdown is refused.
    pub fn ensure_runtime_callback_mutation_allowed(
        &self,
        thread_id: &str,
        stop_completion: bool,
    ) -> Result<()> {
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("callback thread not found: {thread_id}"))?;
        if is_terminal_status(&thread.status) {
            anyhow::bail!(
                "runtime callback mutation is fenced for terminal thread {thread_id} ({})",
                thread.status
            );
        }
        let runtime = g.runtime_db.get_runtime_info(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("runtime row missing for callback thread {thread_id}")
        })?;
        if let Some(intent) = runtime.stop_intent {
            if stop_completion {
                return Ok(());
            }
            anyhow::bail!(
                "runtime callback mutation is fenced after {} request for thread {thread_id}",
                intent.as_str()
            );
        }
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            anyhow::bail!("runtime callback mutation is fenced during daemon shutdown");
        }
        Ok(())
    }

    /// Atomically tombstone an explicit stop against process attachment and
    /// return the identity (if any) that won the attach race before the stop.
    pub fn request_thread_stop(
        &self,
        thread_id: &str,
        intent: runtime_db::StopIntent,
    ) -> Result<RuntimeInfo> {
        let g = self.lock()?;
        g.runtime_db.request_thread_stop(thread_id, intent)
    }

    /// Request a stop only if ordinary execution ownership is still open.
    ///
    /// The gate check and tombstone share the StateStore lock with shutdown's
    /// gate close. Cancellation cleanup therefore either wins before shutdown
    /// and owns the durable stop, or observes the closed gate and leaves the row
    /// untouched for the shutdown coordinator.
    pub fn request_thread_stop_if_admission_open(
        &self,
        thread_id: &str,
        intent: runtime_db::StopIntent,
    ) -> Result<StopIfAdmissionOpenOutcome> {
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found before owner-drop stop: {thread_id}"))?;
        if is_terminal_status(&thread.status) {
            return Ok(StopIfAdmissionOpenOutcome::AlreadyTerminal);
        }
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            return Ok(StopIfAdmissionOpenOutcome::PreservedForShutdown);
        }
        g.runtime_db
            .request_thread_stop(thread_id, intent)
            .map(StopIfAdmissionOpenOutcome::Requested)
    }

    pub fn clear_thread_process_if_matches(
        &self,
        thread_id: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db
            .clear_process_if_matches(thread_id, process_identity)
    }

    pub fn list_attached_thread_ids(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.list_attached_thread_ids()
    }

    /// Read the auto-resume attempt counter for a thread.
    pub fn get_resume_attempts(&self, thread_id: &str) -> Result<u32> {
        let g = self.lock()?;
        g.runtime_db.get_resume_attempts(thread_id)
    }

    /// Atomically bump the auto-resume counter and return the
    /// post-increment value.
    pub fn bump_resume_attempts(&self, thread_id: &str) -> Result<u32> {
        let g = self.lock()?;
        g.runtime_db.bump_resume_attempts(thread_id)
    }

    /// Atomically claim the right to launch a thread. The sole authorization for
    /// a spawn — see [`runtime_db::RuntimeDb::claim_thread_launch`].
    pub fn claim_thread_launch(
        &self,
        thread_id: &str,
        claim_id: &str,
        claimed_by: &str,
        lease_ms: i64,
    ) -> Result<runtime_db::LaunchClaimOutcome> {
        let g = self.lock()?;
        g.runtime_db
            .claim_thread_launch(thread_id, claim_id, claimed_by, lease_ms)
    }

    /// Release a launch claim the caller owns (matched by `claim_id`).
    pub fn release_thread_launch_claim(&self, thread_id: &str, claim_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db
            .release_thread_launch_claim(thread_id, claim_id)
    }

    /// Read the current launch claim, if any — distinguishes an unlaunched
    /// successor from one mid-launch for the reconciler.
    pub fn get_launch_claim(&self, thread_id: &str) -> Result<Option<runtime_db::LaunchClaim>> {
        let g = self.lock()?;
        g.runtime_db.get_launch_claim(thread_id)
    }

    // ── Follow waiters ───────────────────────────────────────────────────

    pub fn reserve_follow(
        &self,
        seed: &runtime_db::NewFollowWaiter,
    ) -> Result<runtime_db::FollowWaiter> {
        let g = self.lock()?;
        g.runtime_db.reserve_follow(seed)
    }

    pub fn set_follow_child(
        &self,
        follow_key: &str,
        item_index: u32,
        item_ref: &str,
        spec_hash: &str,
        child_thread_id: &str,
        child_chain_root_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.set_follow_child(
            follow_key,
            item_index,
            item_ref,
            spec_hash,
            child_thread_id,
            child_chain_root_id,
        )
    }

    pub fn get_follow_child(
        &self,
        follow_key: &str,
        item_index: u32,
    ) -> Result<Option<runtime_db::FollowWaiterChild>> {
        let g = self.lock()?;
        g.runtime_db.get_follow_child(follow_key, item_index)
    }

    pub fn set_follow_parent_successor(
        &self,
        follow_key: &str,
        successor_thread_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .set_follow_parent_successor(follow_key, successor_thread_id)
    }

    pub fn mark_follow_waiting(&self, follow_key: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.mark_follow_waiting(follow_key)
    }

    pub fn mark_follow_resuming(&self, follow_key: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.mark_follow_resuming(follow_key)
    }

    pub fn mark_follow_child_terminal(
        &self,
        child_chain_root_id: &str,
        child_terminal_thread_id: &str,
        child_terminal_status: &str,
        terminal_envelope: &serde_json::Value,
    ) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.mark_follow_child_terminal(
            child_chain_root_id,
            child_terminal_thread_id,
            child_terminal_status,
            terminal_envelope,
        )
    }

    pub fn get_follow_waiter_by_key(
        &self,
        follow_key: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db.get_follow_waiter_by_key(follow_key)
    }

    pub fn get_follow_waiter_by_child_chain(
        &self,
        child_chain_root_id: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db
            .get_follow_waiter_by_child_chain(child_chain_root_id)
    }

    /// The live waiter for a SUSPENDED PARENT thread (the follow issuer), used to
    /// decorate a `continued` thread with its follow lineage.
    pub fn get_follow_waiter_by_parent_thread(
        &self,
        parent_thread_id: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db
            .get_follow_waiter_by_parent_thread(parent_thread_id)
    }

    /// The live waiter whose recorded resume successor is `successor_thread_id`,
    /// used to decorate a follow-resume successor with its live lineage before the
    /// waiter is cleared.
    pub fn get_follow_waiter_by_successor(
        &self,
        successor_thread_id: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db
            .get_follow_waiter_by_successor(successor_thread_id)
    }

    pub fn list_follow_waiters(&self) -> Result<Vec<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db.list_follow_waiters()
    }

    pub fn clear_follow_waiter(&self, follow_key: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.clear_follow_waiter(follow_key)
    }

    /// Delete all launch claims — startup cleanup so a stale claim from a crashed
    /// daemon does not block a reconcile relaunch. See
    /// [`runtime_db::RuntimeDb::clear_all_launch_claims`].
    pub fn clear_all_launch_claims(&self) -> Result<usize> {
        let g = self.lock()?;
        g.runtime_db.clear_all_launch_claims()
    }

    #[tracing::instrument(
        name = "state:append_events",
        skip(self, events),
        fields(
            thread_id = %thread_id,
            chain_root_id = %chain_root_id,
            event_count = events.len(),
        )
    )]
    pub fn append_events(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
    ) -> Result<Vec<PersistedEventRecord>> {
        let has_cas_events = events
            .iter()
            .any(|event| event.storage_class != "ephemeral");
        let _permit = if has_cas_events {
            Some(self.acquire_write_permit()?)
        } else {
            None
        };
        let g = self.lock()?;
        if has_indexed_collection_events(events) && !self.projection_health.is_current() {
            bail!("collection event admission requires a current thread projection");
        }
        append_events_locked(&g, chain_root_id, thread_id, events)
    }

    #[tracing::instrument(
        name = "state:append_events_if_thread_running",
        skip(self, events),
        fields(
            thread_id = %thread_id,
            chain_root_id = %chain_root_id,
            event_count = events.len(),
        )
    )]
    pub fn append_events_if_thread_running(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
    ) -> Result<Option<Vec<PersistedEventRecord>>> {
        let has_cas_events = events
            .iter()
            .any(|event| event.storage_class != "ephemeral");
        let _permit = if has_cas_events {
            Some(self.acquire_write_permit()?)
        } else {
            None
        };
        let g = self.lock()?;
        if has_indexed_collection_events(events) && !self.projection_health.is_current() {
            bail!("collection event admission requires a current thread projection");
        }
        let Some(thread) = g.state_db.get_thread(thread_id)? else {
            return Ok(None);
        };
        if thread.status != "running" {
            return Ok(None);
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing while appending events: {thread_id}"))?;
        if runtime.stop_intent.is_some()
            || !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            return Ok(None);
        }

        append_events_locked(&g, chain_root_id, thread_id, events).map(Some)
    }

    /// The thread a live tail of `chain_root_id` should currently follow: the
    /// owner of the chain's highest-`chain_seq` event. `None` when the chain
    /// has no events yet.
    pub fn chain_head_thread(&self, chain_root_id: &str) -> Result<Option<String>> {
        let g = self.lock()?;
        queries::chain_head_thread(g.state_db.projection(), chain_root_id)
    }

    pub fn replay_events(
        &self,
        chain_root_id: &str,
        thread_id: Option<&str>,
        after_seq: Option<i64>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<PersistedEventPage> {
        let g = self.lock()?;
        let page = queries::replay_events_bounded(
            g.state_db.projection(),
            chain_root_id,
            thread_id,
            after_seq,
            limit,
            max_serialized_bytes,
        )?;
        drop(g);
        let events = page
            .rows
            .into_iter()
            .map(|row| {
                let payload: Value = serde_json::from_slice(&row.payload).with_context(|| {
                    format!(
                        "malformed JSON payload for event {} (chain_seq {})",
                        row.event_id, row.chain_seq
                    )
                })?;
                Ok(PersistedEventRecord {
                    event_id: row.event_id,
                    event_hash: Some(row.event_hash),
                    chain_root_id: row.chain_root_id,
                    chain_seq: row.chain_seq,
                    thread_id: row.thread_id,
                    thread_seq: row.thread_seq,
                    event_type: row.event_type,
                    storage_class: row.durability,
                    ts: row.ts,
                    prev_chain_event_hash: row.prev_chain_event_hash,
                    prev_thread_event_hash: row.prev_thread_event_hash,
                    payload,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(PersistedEventPage {
            events,
            has_more: page.has_more,
        })
    }

    /// Latest durable events across every thread on the node — the feed
    /// behind the node activity lens. `exclude_types` is caller-declared
    /// (content decides what counts as noise), never a baked-in vocabulary.
    pub fn latest_node_events(
        &self,
        limit: usize,
        exclude_types: &[String],
    ) -> Result<Vec<PersistedEventRecord>> {
        let g = self.lock()?;
        let event_rows =
            queries::latest_node_events(g.state_db.projection(), limit, exclude_types)?;
        event_rows
            .into_iter()
            .map(|row| {
                let payload: Value = serde_json::from_slice(&row.payload).with_context(|| {
                    format!(
                        "malformed JSON payload for event {} (chain_seq {})",
                        row.event_id, row.chain_seq
                    )
                })?;
                Ok(PersistedEventRecord {
                    event_id: row.event_id,
                    event_hash: Some(row.event_hash),
                    chain_root_id: row.chain_root_id,
                    chain_seq: row.chain_seq,
                    thread_id: row.thread_id,
                    thread_seq: row.thread_seq,
                    event_type: row.event_type,
                    storage_class: row.durability,
                    ts: row.ts,
                    prev_chain_event_hash: row.prev_chain_event_hash,
                    prev_thread_event_hash: row.prev_thread_event_hash,
                    payload,
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn latest_thread_events(
        &self,
        thread_id: &str,
        limit: usize,
    ) -> Result<Vec<PersistedEventRecord>> {
        let g = self.lock()?;
        let event_rows = queries::latest_thread_events(g.state_db.projection(), thread_id, limit)?;
        event_rows
            .into_iter()
            .map(|row| {
                let payload: Value = serde_json::from_slice(&row.payload).with_context(|| {
                    format!(
                        "malformed JSON payload for event {} (chain_seq {})",
                        row.event_id, row.chain_seq
                    )
                })?;
                Ok(PersistedEventRecord {
                    event_id: row.event_id,
                    event_hash: Some(row.event_hash),
                    chain_root_id: row.chain_root_id,
                    chain_seq: row.chain_seq,
                    thread_id: row.thread_id,
                    thread_seq: row.thread_seq,
                    event_type: row.event_type,
                    storage_class: row.durability,
                    ts: row.ts,
                    prev_chain_event_hash: row.prev_chain_event_hash,
                    prev_thread_event_hash: row.prev_thread_event_hash,
                    payload,
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn append_bundle_event(
        &self,
        request: ryeos_state::BundleEventAppendRequest,
    ) -> Result<ryeos_state::BundleEventAppendResult> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        g.state_db.append_bundle_event(request, g.signer.as_ref())
    }

    pub fn read_bundle_event_chain_page(
        &self,
        bundle_id: &str,
        event_kind: &str,
        chain_id: &str,
        cursor: Option<&str>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<ryeos_state::BundleEventChainPage> {
        let g = self.lock()?;
        g.state_db.read_bundle_event_chain_page(
            bundle_id,
            event_kind,
            chain_id,
            cursor,
            limit,
            max_serialized_bytes,
        )
    }

    pub fn scan_bundle_events_page(
        &self,
        bundle_id: &str,
        event_kind: &str,
        cursor: Option<&ryeos_state::BundleEventScanCursor>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<ryeos_state::BundleEventScanPage> {
        let g = self.lock()?;
        g.state_db.scan_bundle_events_page(
            bundle_id,
            event_kind,
            cursor,
            limit,
            max_serialized_bytes,
        )
    }

    pub fn submit_command(&self, cmd: &NewCommandRecord) -> Result<CommandRecord> {
        let g = self.lock()?;
        g.runtime_db.submit_command(cmd)
    }

    pub fn claim_commands(
        &self,
        thread_id: &str,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<Vec<CommandRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .claim_commands(thread_id, limit, max_serialized_bytes)
    }

    pub fn reset_resume_attempts(&self, thread_id: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.reset_resume_attempts(thread_id)
    }

    /// Enqueue a detached child chain into a launch window and admit as many
    /// queued members as the window width (and optional global live ceiling)
    /// allow. Returns the chain roots admitted NOW — the caller launches
    /// them; the enqueued child is queued iff its id is absent.
    pub fn launch_window_enqueue(
        &self,
        child_chain_root_id: &str,
        window_key: &str,
        width: u32,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db
            .launch_window_insert(child_chain_root_id, window_key, width, now_ms)?;
        g.runtime_db
            .launch_window_admit(window_key, global_live_limit, now_ms)
    }

    /// Repair membership without admitting it; used when launch metadata proves
    /// the child was originally windowed but the membership write was lost.
    pub fn launch_window_insert_only(
        &self,
        child_chain_root_id: &str,
        window_key: &str,
        width: u32,
        now_ms: i64,
    ) -> Result<bool> {
        self.lock()?
            .runtime_db
            .launch_window_insert(child_chain_root_id, window_key, width, now_ms)
    }

    /// Release a window slot for a chain that reached a hard terminal and
    /// admit the window's next queued members (returned for launching).
    pub fn launch_window_release(
        &self,
        child_chain_root_id: &str,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db
            .launch_window_release(child_chain_root_id, global_live_limit, now_ms)
    }

    pub fn launch_window_is_queued(&self, child_chain_root_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.launch_window_is_queued(child_chain_root_id)
    }

    pub fn launch_window_cancel_queued(
        &self,
        chain_roots: &[String],
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let mut g = self.lock()?;
        g.runtime_db
            .launch_window_cancel_queued(chain_roots, now_ms)
    }

    pub fn launch_window_cancel_members(
        &self,
        chain_roots: &[String],
        now_ms: i64,
    ) -> Result<Vec<String>> {
        self.lock()?
            .runtime_db
            .launch_window_cancel_members(chain_roots, now_ms)
    }

    pub fn launch_window_is_cancelled(&self, chain_root: &str) -> Result<bool> {
        self.lock()?
            .runtime_db
            .launch_window_is_cancelled(chain_root)
    }

    pub fn list_cancelled_window_members(&self) -> Result<Vec<String>> {
        self.lock()?.runtime_db.launch_window_cancelled_members()
    }

    pub fn discard_window_member(&self, chain_root: &str) -> Result<()> {
        self.lock()?
            .runtime_db
            .launch_window_discard_member(chain_root)
    }

    pub fn launch_window_is_member(&self, child_chain_root_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.launch_window_is_member(child_chain_root_id)
    }

    pub fn launch_window_launched_members(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.launch_window_launched_members()
    }

    pub fn launch_window_keys_with_queue(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.launch_window_keys_with_queue()
    }

    pub fn launch_window_admit(
        &self,
        window_key: &str,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db
            .launch_window_admit(window_key, global_live_limit, now_ms)
    }

    pub fn complete_command(
        &self,
        command_id: i64,
        status: &str,
        result: Option<&Value>,
    ) -> Result<CommandRecord> {
        let g = self.lock()?;
        g.runtime_db.complete_command(command_id, status, result)
    }

    /// Read one command by id, or `None` if it does not exist.
    pub fn get_command(&self, command_id: i64) -> Result<Option<CommandRecord>> {
        let g = self.lock()?;
        g.runtime_db.get_command(command_id)
    }

    /// Whether a `kill` command was ever submitted for `thread_id` (the launcher's
    /// kill-intent marker). See [`RuntimeDb::thread_has_kill_command`].
    pub fn thread_has_kill_command(&self, thread_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.thread_has_kill_command(thread_id)
    }

    /// Settle every still-open command for a finalized thread (fulfilled →
    /// `completed`, else `rejected`), returning the affected records so waiters
    /// can be woken. See [`RuntimeDb::settle_open_commands`].
    pub fn settle_open_commands(
        &self,
        thread_id: &str,
        terminal_status: &str,
    ) -> Result<Vec<CommandRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .settle_open_commands(thread_id, terminal_status)
    }

    /// Record that `parent_thread_id` spawned `child_thread_id` (operational
    /// lineage for cancel/kill cascade). Idempotent on the child. If the parent
    /// was already stop-tombstoned, atomically inherit that monotonic intent on
    /// the child before releasing the store lock, closing the late-link race.
    pub fn record_child_link(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
        relation: &str,
    ) -> Result<Option<StopIntent>> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            // The shutdown gate may close after a callback created its child but
            // before it reached this second durable mutation. Tombstone the child
            // first so no later attach can win, then preserve operational lineage
            // on a best-effort basis. Link persistence is useful for inspection,
            // but must not prevent the already-durable stop from settling.
            let stopped = g
                .runtime_db
                .request_thread_stop(child_thread_id, StopIntent::Cancel)?;
            if let Err(error) =
                g.runtime_db
                    .record_child_link(parent_thread_id, child_thread_id, relation)
            {
                tracing::warn!(
                    parent_thread_id,
                    child_thread_id,
                    relation,
                    error = %error,
                    "failed to preserve child lineage after shutdown tombstone"
                );
            }
            return Ok(Some(stopped.stop_intent.unwrap_or(StopIntent::Cancel)));
        }
        let parent = g
            .state_db
            .get_thread(parent_thread_id)?
            .ok_or_else(|| anyhow::anyhow!("parent thread not found: {parent_thread_id}"))?;
        let mut inherited_stop = g
            .runtime_db
            .get_runtime_info(parent_thread_id)?
            .ok_or_else(|| anyhow::anyhow!("parent runtime row missing: {parent_thread_id}"))?
            .stop_intent;
        // A dispatch admitted immediately before its parent finalized may reach
        // this atomic linkage point afterward. Preserve the lineage, but stop
        // the freshly-created child before it can attach. Continuations are
        // different: their predecessor is intentionally terminal/continued.
        if relation == "dispatch" && is_terminal_status(&parent.status) {
            inherited_stop = Some(StopIntent::Cancel);
        }
        g.runtime_db
            .record_child_link(parent_thread_id, child_thread_id, relation)?;
        if let Some(intent) = inherited_stop {
            g.runtime_db.request_thread_stop(child_thread_id, intent)?;
        }
        Ok(inherited_stop)
    }

    /// Every transitive descendant thread id of `root_thread_id`, breadth-first
    /// in spawn order (excludes `root`).
    pub fn descendant_thread_ids(&self, root_thread_id: &str) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.descendant_thread_ids(root_thread_id)
    }

    pub fn get_facets(&self, thread_id: &str) -> Result<Vec<(String, String)>> {
        let facet_rows = {
            let g = self.lock()?;
            let (items, content_bytes) =
                queries::thread_facet_stats(g.state_db.projection(), thread_id)?;
            ensure_facet_collection_bounds(thread_id, items, content_bytes)?;
            queries::get_facets_bounded(
                g.state_db.projection(),
                thread_id,
                MAX_THREAD_FACET_ITEMS,
                MAX_THREAD_FACET_KEY_BYTES,
                MAX_THREAD_FACET_VALUE_BYTES,
                MAX_THREAD_FACET_CONTENT_BYTES,
            )?
        };
        let mut facets = Vec::with_capacity(facet_rows.len());
        let mut response_bytes = b"{}".len();
        for row in facet_rows {
            let value = String::from_utf8(row.value)
                .with_context(|| format!("thread {thread_id} facet value is not UTF-8"))?;
            let key_bytes = serde_json::to_vec(&row.key)?.len();
            let value_bytes = serde_json::to_vec(&value)?.len();
            response_bytes = response_bytes
                .checked_add(key_bytes)
                .and_then(|bytes| bytes.checked_add(value_bytes))
                .and_then(|bytes| bytes.checked_add(1)) // colon
                .and_then(|bytes| bytes.checked_add(usize::from(!facets.is_empty())))
                .ok_or_else(|| anyhow!("thread facet response size overflow"))?;
            if response_bytes > MAX_THREAD_FACET_RESPONSE_BYTES {
                bail!(
                    "thread {thread_id} facet response exceeds the {MAX_THREAD_FACET_RESPONSE_BYTES}-byte maximum"
                );
            }
            facets.push((row.key, value));
        }
        Ok(facets)
    }

    /// A graph thread's current `(node, step)` from its latest
    /// `graph_step_started` — the cheap live "where is it now". See
    /// [`queries::current_graph_node`].
    pub fn current_graph_node(&self, thread_id: &str) -> Result<Option<(String, u32)>> {
        let g = self.lock()?;
        queries::current_graph_node(g.state_db.projection(), thread_id)
    }
}

/// Reconcile process fields introduced before durable process identities.
///
/// Terminal threads and rows absent from the authoritative projection cannot
/// be resumed, so their pid/pgid values are non-authoritative history and are
/// safe to clear. A nonterminal row may still name a live process incarnation;
/// without its boot/start-time identity RyeOS cannot safely signal it or launch
/// a replacement beside it, so startup remains fail-closed for those rows.
fn reconcile_unauthenticated_process_fields(
    state_db: &StateDb,
    runtime_db: &runtime_db::RuntimeDb,
) -> Result<()> {
    const DIAGNOSTIC_SAMPLE_LIMIT: usize = 8;

    let mut after_thread_id: Option<String> = None;
    let mut cleared_total = 0usize;
    let mut nonterminal_total = 0usize;
    let mut nonterminal_sample = Vec::new();

    loop {
        let rows = runtime_db
            .unauthenticated_process_rows_after(after_thread_id.as_deref())
            .context("scan runtime process fields without durable identity")?;
        if rows.is_empty() {
            break;
        }
        after_thread_id = rows.last().map(|row| row.thread_id.clone());

        let thread_ids = rows
            .iter()
            .map(|row| row.thread_id.clone())
            .collect::<Vec<_>>();
        let projected_statuses = queries::get_threads_many(state_db.projection(), &thread_ids)?
            .into_iter()
            .map(|thread| (thread.thread_id, thread.status))
            .collect::<HashMap<_, _>>();

        let mut clearable = Vec::with_capacity(rows.len());
        for row in rows {
            match projected_statuses.get(&row.thread_id) {
                None => clearable.push(row.thread_id),
                Some(status) if is_terminal_status(status) => clearable.push(row.thread_id),
                Some(status) => {
                    nonterminal_total += 1;
                    if nonterminal_sample.len() < DIAGNOSTIC_SAMPLE_LIMIT {
                        nonterminal_sample.push(format!("{} ({status})", row.thread_id));
                    }
                }
            }
        }
        cleared_total += runtime_db
            .clear_unauthenticated_process_fields(&clearable)
            .context("clear non-authoritative process fields from terminal runtime history")?;
    }

    if cleared_total > 0 {
        tracing::info!(
            rows = cleared_total,
            "cleared non-authoritative pid/pgid fields from terminal or unprojected runtime history"
        );
    }
    if nonterminal_total > 0 {
        bail!(
            "runtime state contains {nonterminal_total} nonterminal process row(s) without durable identity; refusing startup because their live process incarnation cannot be proven (sample: {})",
            nonterminal_sample.join(", ")
        );
    }
    Ok(())
}

/// Whether a thread's persisted `status` is terminal. The canonical predicate —
/// `ThreadTerminalStatus`'s string mapping omits daemon-owned `timed_out`, so it
/// is not usable for this; callers outside this module (e.g. the cancel cascade)
/// reuse this rather than re-listing the statuses.
pub fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
    )
}

fn terminal_event_type(status: &str) -> Result<&'static str> {
    match status {
        "completed" => Ok("thread_completed"),
        "failed" => Ok("thread_failed"),
        "cancelled" => Ok("thread_cancelled"),
        "killed" => Ok("thread_killed"),
        "timed_out" => Ok("thread_timed_out"),
        "continued" => Ok("thread_continued"),
        other => bail!("invalid terminal event status: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};
    use tempfile::tempdir;

    struct LocalTestSigner;

    impl Signer for LocalTestSigner {
        fn sign(&self, _data: &[u8]) -> Vec<u8> {
            vec![1; 64]
        }

        fn fingerprint(&self) -> &str {
            "fp:test"
        }
    }

    fn test_store() -> StateStore {
        let tmp = tempdir().expect("tempdir").keep();
        StateStore::new(
            tmp.join("state"),
            tmp.join("runtime.sqlite3"),
            Arc::new(LocalTestSigner),
            WriteBarrier::new(),
        )
        .expect("state store")
    }

    #[test]
    fn startup_clears_unauthenticated_process_fields_only_from_inactive_history() {
        let tmp = tempdir().expect("tempdir");
        let state_dir = tmp.path().join("state");
        let runtime_path = tmp.path().join("runtime.sqlite3");

        {
            let store = StateStore::new(
                state_dir.clone(),
                runtime_path.clone(),
                Arc::new(LocalTestSigner),
                WriteBarrier::new(),
            )
            .expect("initial state store");
            store
                .create_thread(&thread_record("T-terminal", "T-terminal"))
                .expect("terminal test thread");
            store
                .finalize_if_nonterminal(
                    "T-terminal",
                    &FinalizeThreadRecord {
                        status: "completed".into(),
                        outcome_code: None,
                        result_json: None,
                        error_json: None,
                        artifacts: Vec::new(),
                        final_cost: None,
                    },
                )
                .expect("finalize test thread");
        }

        {
            let runtime = runtime_db::RuntimeDb::open(&runtime_path).expect("runtime db");
            runtime
                .insert_thread_runtime("T-unprojected", "T-unprojected")
                .expect("unprojected runtime row");
        }
        {
            let conn = Connection::open(&runtime_path).expect("raw runtime db");
            for thread_id in ["T-terminal", "T-unprojected"] {
                conn.execute(
                    "UPDATE thread_runtime SET pid = ?2, pgid = ?3 WHERE thread_id = ?1",
                    params![thread_id, 101_i64, 101_i64],
                )
                .expect("seed process fields without identity");
            }
        }

        let store = StateStore::new(
            state_dir,
            runtime_path,
            Arc::new(LocalTestSigner),
            WriteBarrier::new(),
        )
        .expect("reconciled state store");
        let inner = store.lock().expect("state store lock");
        for thread_id in ["T-terminal", "T-unprojected"] {
            let runtime = inner
                .runtime_db
                .get_runtime_info(thread_id)
                .expect("runtime info")
                .expect("runtime row");
            assert_eq!(runtime.pid, None);
            assert_eq!(runtime.pgid, None);
            assert_eq!(runtime.process_identity, None);
        }
    }

    #[test]
    fn startup_refuses_nonterminal_process_fields_without_durable_identity() {
        let tmp = tempdir().expect("tempdir");
        let state_dir = tmp.path().join("state");
        let runtime_path = tmp.path().join("runtime.sqlite3");

        {
            let store = StateStore::new(
                state_dir.clone(),
                runtime_path.clone(),
                Arc::new(LocalTestSigner),
                WriteBarrier::new(),
            )
            .expect("initial state store");
            store
                .create_thread(&thread_record("T-active", "T-active"))
                .expect("active test thread");
        }
        {
            let conn = Connection::open(&runtime_path).expect("raw runtime db");
            conn.execute(
                "UPDATE thread_runtime SET pid = ?2, pgid = ?3 WHERE thread_id = ?1",
                params!["T-active", 202_i64, 202_i64],
            )
            .expect("seed active process fields without identity");
        }

        let error = StateStore::new(
            state_dir,
            runtime_path.clone(),
            Arc::new(LocalTestSigner),
            WriteBarrier::new(),
        )
        .expect_err("active process state must fail closed");
        assert!(error
            .to_string()
            .contains("1 nonterminal process row(s) without durable identity"));

        let conn = Connection::open(runtime_path).expect("raw runtime db");
        let process_fields = conn
            .query_row(
                "SELECT pid, pgid FROM thread_runtime WHERE thread_id = ?1",
                params!["T-active"],
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .expect("active process fields");
        assert_eq!(process_fields, (Some(202), Some(202)));
    }

    #[test]
    fn direct_projection_access_fails_closed_while_repair_is_pending() {
        let store = test_store();
        ryeos_state::ProjectionRepairSink::request_repair(
            &*store.projection_health(),
            ryeos_state::ProjectionRepairRequest {
                chain_root_id: "T-root".into(),
                committed_head_hash: "head".into(),
                operation: "append_events",
                error: "projection failed".into(),
            },
        );
        let error = store
            .with_projection(|_| Ok(()))
            .expect_err("stale projection read must fail");
        assert!(error.to_string().contains("not current"));
    }

    fn thread_record(thread_id: &str, chain_root_id: &str) -> NewThreadRecord {
        NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: chain_root_id.to_string(),
            kind: "directive".to_string(),
            item_ref: "directive:test".to_string(),
            executor_ref: "native:test".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("fp:test".to_string()),
            project_root: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
        }
    }

    #[test]
    fn trace_branch_does_not_project_ordinary_upstream_edge() {
        let store = test_store();
        store
            .create_thread(&thread_record("T-root", "T-root"))
            .expect("root thread");

        let branch = thread_record("T-branch", "T-root");
        let persisted = store
            .create_trace_branch(
                &branch,
                json!({
                    "relation": "trace_branch",
                    "child_thread_id": "T-branch",
                    "parent_event_ref": {"chain_root_id": "T-root"},
                    "state_anchor_ref": {"chain_root_id": "T-root"}
                }),
            )
            .expect("trace branch");

        let child = store
            .get_thread("T-branch")
            .expect("get branch")
            .expect("branch thread");
        assert_eq!(child.upstream_thread_id, None);
        assert!(store.list_chain_edges("T-root").expect("edges").is_empty());
        assert_eq!(persisted.len(), 2);
        assert_eq!(
            persisted[1].event_type,
            ryeos_state::event_types::EDGE_RECORDED
        );
        assert_eq!(persisted[1].payload["relation"], json!("trace_branch"));
    }

    #[test]
    fn trace_branch_duplicate_explicit_child_id_does_not_append_events() {
        let store = test_store();
        store
            .create_thread(&thread_record("T-root", "T-root"))
            .expect("root thread");

        let branch = thread_record("T-branch", "T-root");
        store
            .create_trace_branch(
                &branch,
                json!({
                    "relation": "trace_branch",
                    "child_thread_id": "T-branch",
                    "parent_event_ref": {"chain_root_id": "T-root"},
                    "state_anchor_ref": {"chain_root_id": "T-root"}
                }),
            )
            .expect("trace branch");

        let head_after_first = {
            let g = store.lock().expect("lock");
            g.state_db
                .read_generic_head_ref("chains", "T-root")
                .expect("read chain head")
                .expect("chain head")
                .target_hash
        };

        let err = store
            .create_trace_branch(
                &branch,
                json!({
                    "relation": "trace_branch",
                    "child_thread_id": "T-branch",
                    "parent_event_ref": {"chain_root_id": "T-root"},
                    "state_anchor_ref": {"chain_root_id": "T-root"}
                }),
            )
            .expect_err("duplicate child id should fail");

        assert!(
            err.to_string().contains("already exists"),
            "unexpected error: {err:#}"
        );

        let head_after_duplicate = {
            let g = store.lock().expect("lock");
            g.state_db
                .read_generic_head_ref("chains", "T-root")
                .expect("read chain head")
                .expect("chain head")
                .target_hash
        };
        assert_eq!(head_after_duplicate, head_after_first);

        let events = store
            .replay_events("T-root", Some("T-branch"), None, 10, 1024 * 1024)
            .expect("branch replay");
        assert_eq!(events.events.len(), 2);
        assert_eq!(
            events.events[0].event_type,
            ryeos_state::event_types::THREAD_CREATED
        );
        assert_eq!(
            events.events[1].event_type,
            ryeos_state::event_types::EDGE_RECORDED
        );
    }
}
