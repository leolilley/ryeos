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

fn default_replay_limit() -> usize {
    200
}

impl EventStoreService {
    pub fn new(
        state_store: Arc<StateStore>,
    ) -> Self {
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
        let thread = self
            .state_store
            .get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread not found: {}", params.thread_id))?;

        let events = params
            .events
            .iter()
            .map(|event| {
                validate_event_type(&event.event_type)?;
                validate_storage_class(&event.storage_class)?;
                Ok(NewEventRecord {
                    event_type: event.event_type.clone(),
                    storage_class: event.storage_class.clone(),
                    payload: event.payload.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let persisted = self.state_store
            .append_events(&thread.chain_root_id, &thread.thread_id, &events)?;

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

        let events = self.state_store.replay_events(
            &chain_root_id,
            thread_id.as_deref(),
            params.after_chain_seq,
            params.limit,
        )?;
        let next_cursor = if events.len() == params.limit {
            events.last().map(|event| event.chain_seq)
        } else {
            None
        };

        Ok(EventReplayResult {
            events,
            next_cursor,
        })
    }
}

/// V5.5 D11: delegate to the typed `RuntimeEventType` enum.
///
/// `ryeos-runtime/src/events.rs` is the single source of truth for
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
