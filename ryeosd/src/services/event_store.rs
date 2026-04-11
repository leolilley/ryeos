use std::sync::Arc;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::broker::LiveBroker;
use crate::db::{Database, NewEventRecord, PersistedEventRecord};

#[derive(Debug, Clone)]
pub struct EventStoreService {
    db: Arc<Database>,
    broker: Arc<LiveBroker>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventAppendItem {
    pub event_type: String,
    pub storage_class: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventAppendParams {
    pub thread_id: String,
    pub event: EventAppendItem,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventAppendBatchParams {
    pub thread_id: String,
    pub events: Vec<EventAppendItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventAppendBatchResult {
    pub persisted: Vec<PersistedEventRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub fn new(db: Arc<Database>, broker: Arc<LiveBroker>) -> Self {
        Self { db, broker }
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

    pub fn append_batch(&self, params: &EventAppendBatchParams) -> Result<EventAppendBatchResult> {
        let thread = self
            .db
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

        let persisted = self
            .db
            .append_events(&thread.chain_root_id, &thread.thread_id, &events)?;
        self.broker.publish_batch(&persisted);

        Ok(EventAppendBatchResult { persisted })
    }

    pub fn replay(&self, params: &EventReplayParams) -> Result<EventReplayResult> {
        let (chain_root_id, thread_id) = match (&params.chain_root_id, &params.thread_id) {
            (Some(chain_root_id), thread_id) => (chain_root_id.clone(), thread_id.clone()),
            (None, Some(thread_id)) => {
                let thread = self
                    .db
                    .get_thread(thread_id)?
                    .ok_or_else(|| anyhow::anyhow!("thread not found: {thread_id}"))?;
                (thread.chain_root_id, Some(thread_id.clone()))
            }
            (None, None) => bail!("events.replay requires chain_root_id or thread_id"),
        };

        let events = self.db.replay_events(
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

    pub fn live_catch_up(
        &self,
        chain_root_id: &str,
        thread_id: Option<&str>,
        after_chain_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.db
            .list_events(chain_root_id, thread_id, after_chain_seq, limit)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PersistedEventRecord> {
        self.broker.subscribe()
    }

    pub fn publish_persisted_batch(&self, events: &[PersistedEventRecord]) {
        self.broker.publish_batch(events)
    }
}

fn validate_event_type(event_type: &str) -> Result<()> {
    match event_type {
        "thread_created"
        | "thread_started"
        | "thread_completed"
        | "thread_failed"
        | "thread_cancelled"
        | "thread_killed"
        | "thread_timed_out"
        | "thread_continued"
        | "edge_recorded"
        | "child_thread_spawned"
        | "continuation_requested"
        | "continuation_accepted"
        | "command_submitted"
        | "command_claimed"
        | "command_completed"
        | "budget_reserved"
        | "budget_reported"
        | "budget_released"
        | "stream_opened"
        | "token_delta"
        | "stream_snapshot"
        | "stream_closed"
        | "artifact_published"
        | "thread_reconciled"
        | "orphan_process_killed"
        | "system_prompt"
        | "context_injected"
        | "cognition_in"
        | "cognition_out"
        | "cognition_reasoning"
        | "tool_call_start"
        | "tool_call_result" => Ok(()),
        other if other.trim().is_empty() => bail!("event_type must not be empty"),
        other => bail!("invalid event_type: {other}"),
    }
}

fn validate_storage_class(storage_class: &str) -> Result<()> {
    match storage_class {
        "indexed" | "journal_only" => Ok(()),
        other => bail!("invalid storage_class: {other}"),
    }
}
