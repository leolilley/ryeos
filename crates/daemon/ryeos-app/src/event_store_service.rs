use std::sync::Arc;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state_store::{NewEventRecord, PersistedEventRecord, StateStore};

#[derive(Debug, Clone)]
pub struct EventStoreService {
    state_store: Arc<StateStore>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventAppendItem {
    pub event_type: String,
    pub storage_class: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventAppendParams {
    pub thread_id: String,
    pub event: EventAppendItem,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventAppendBatchParams {
    pub thread_id: String,
    pub events: Vec<EventAppendItem>,
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
const MAX_RUNTIME_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_RUNTIME_EVENT_BATCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_RUNTIME_EVENT_BATCH_ITEMS: usize = 64;

fn default_replay_limit() -> usize {
    DEFAULT_REPLAY_LIMIT
}

impl EventStoreService {
    pub fn new(state_store: Arc<StateStore>) -> Self {
        Self { state_store }
    }

    pub fn append(&self, params: &EventAppendParams) -> Result<PersistedEventRecord> {
        let result = self.append_batch(&EventAppendBatchParams {
            thread_id: params.thread_id.clone(),
            events: vec![params.event.clone()],
        })?;
        result
            .persisted
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("append returned no persisted event"))
    }

    #[tracing::instrument(
        level = "debug",
        name = "event:append_batch",
        skip(self, params),
        fields(
            thread_id = %params.thread_id,
            count = params.events.len(),
        )
    )]
    pub fn append_batch(&self, params: &EventAppendBatchParams) -> Result<EventAppendBatchResult> {
        if params.events.is_empty() {
            bail!("event batch must not be empty");
        }
        if params.events.len() > MAX_RUNTIME_EVENT_BATCH_ITEMS {
            bail!(
                "event batch contains {} items (max {})",
                params.events.len(),
                MAX_RUNTIME_EVENT_BATCH_ITEMS
            );
        }
        let mut batch_bytes = 0usize;
        for event in &params.events {
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
        }
        if batch_bytes > MAX_RUNTIME_EVENT_BATCH_BYTES {
            bail!(
                "event batch payload is {} bytes (max {})",
                batch_bytes,
                MAX_RUNTIME_EVENT_BATCH_BYTES
            );
        }
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

        let events = params
            .events
            .iter()
            .map(|event| {
                validate_event_type(&event.event_type)?;
                validate_storage_class(&event.storage_class)?;
                validate_event_storage_class(
                    &event.event_type,
                    &event.storage_class,
                    &event.payload,
                )?;
                Ok(NewEventRecord {
                    event_type: event.event_type.clone(),
                    storage_class: event.storage_class.clone(),
                    payload: event.payload.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let persisted = self
            .state_store
            .append_events_if_thread_running(&thread.chain_root_id, &thread.thread_id, &events)?
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

        let page = self.state_store.replay_events(
            &chain_root_id,
            thread_id.as_deref(),
            params.after_chain_seq,
            params.limit,
            MAX_REPLAY_SERIALIZED_BYTES,
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

/// V5.5 D11: delegate to the typed `RuntimeEventType` enum.
///
/// `crates/engine/ryeos-runtime/src/events.rs` is the single source of truth for
/// the event vocabulary. Both daemon validators and runtime emitters
/// consume the same enum, so adding a variant cannot drift between
/// the two — the producer and consumer surfaces both fail to compile
/// (or accept the new variant) atomically.
fn validate_event_type(event_type: &str) -> Result<()> {
    ryeos_runtime::RuntimeEventType::parse(event_type).map(|_| ())
}

fn validate_storage_class(storage_class: &str) -> Result<()> {
    ryeos_runtime::StorageClass::parse(storage_class).map(|_| ())
}

fn validate_event_storage_class(
    event_type: &str,
    storage_class: &str,
    payload: &Value,
) -> Result<()> {
    if ryeos_runtime::StorageClass::parse(storage_class)? != ryeos_runtime::StorageClass::Ephemeral
    {
        return Ok(());
    }

    if is_ephemeral_allowed(event_type, payload) {
        Ok(())
    } else {
        bail!("event_type {event_type} cannot use ephemeral storage_class")
    }
}

fn is_ephemeral_allowed(event_type: &str, payload: &Value) -> bool {
    use ryeos_runtime::{RuntimeEventType, StorageClass};
    // Lock-step with the runtime's `storage_class_for_payload`. The event-type
    // vocabulary is the typed enum, not raw strings.
    match RuntimeEventType::parse(event_type) {
        // Types whose nature is always live-only (token deltas, stream snapshots,
        // reasoning, foreach progress) — sourced from the enum so a new ephemeral
        // variant is covered automatically.
        Ok(t) if t.storage_class() == StorageClass::Ephemeral => true,
        // cognition_out is indexed by default, but progressive streaming payloads
        // (delta / tool_use_partial / complete tool_use) are live-only. The durable
        // tool-call record is the turn-complete `cognition_out{tool_calls}`, never
        // the mid-stream `tool_use`. (Payload keys are JSON field names, not event
        // types, so they stay string-keyed.)
        Ok(RuntimeEventType::CognitionOut) => {
            payload.get("delta").is_some()
                || payload.get("tool_use_partial").is_some()
                || payload.get("tool_use").is_some()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn progressive_cognition_out_allows_ephemeral() {
        // Lock-step with the runtime: delta / tool_use_partial / complete tool_use.
        for payload in [
            json!({"delta": "hi"}),
            json!({"tool_use_partial": {"id": "x"}}),
            json!({"tool_use": {"id": "x", "name": "f", "arguments": {}}}),
        ] {
            assert!(
                is_ephemeral_allowed("cognition_out", &payload),
                "cognition_out {payload} should allow ephemeral"
            );
            // And the validator accepts an ephemeral storage_class for it.
            assert!(validate_event_storage_class("cognition_out", "ephemeral", &payload).is_ok());
        }
    }

    #[test]
    fn durable_cognition_out_rejects_ephemeral() {
        // A turn-complete cognition_out (tool_calls array) or interrupted seal must
        // be indexed; claiming ephemeral is rejected.
        for payload in [
            json!({"content": "done", "tool_calls": []}),
            json!({"content": "partial", "interrupted": true}),
        ] {
            assert!(!is_ephemeral_allowed("cognition_out", &payload));
            assert!(validate_event_storage_class("cognition_out", "ephemeral", &payload).is_err());
            // Indexed is always fine.
            assert!(validate_event_storage_class("cognition_out", "indexed", &payload).is_ok());
        }
    }

    #[test]
    fn always_ephemeral_types_come_from_the_enum() {
        for et in ["token_delta", "stream_snapshot", "cognition_reasoning"] {
            assert!(is_ephemeral_allowed(et, &json!({})));
        }
    }
}
