use std::sync::Arc;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{CommandRecord, Database, NewCommandRecord};
use crate::services::event_store::EventStoreService;

#[derive(Debug, Clone)]
pub struct CommandService {
    db: Arc<Database>,
    state_store: Arc<crate::state_store::StateStore>,
    events: Arc<EventStoreService>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSubmitParams {
    pub thread_id: String,
    pub command_type: String,
    #[serde(default)]
    pub requested_by: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandClaimParams {
    pub thread_id: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandClaimResult {
    pub commands: Vec<CommandRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandCompleteParams {
    pub command_id: i64,
    pub status: String,
    #[serde(default)]
    pub result: Option<Value>,
}

impl CommandService {
    pub fn new(
        db: Arc<Database>,
        state_store: Arc<crate::state_store::StateStore>,
        events: Arc<EventStoreService>,
    ) -> Self {
        Self { db, state_store, events }
    }

    pub fn submit(&self, params: &CommandSubmitParams) -> Result<CommandRecord> {
        validate_command_type(&params.command_type)?;

        let thread = self
            .db
            .get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread not found: {}", params.thread_id))?;
        if !thread
            .allowed_actions
            .iter()
            .any(|action| action == &params.command_type)
        {
            bail!(
                "command {} is not allowed for thread {} in status {}",
                params.command_type,
                params.thread_id,
                thread.status
            );
        }

        let (command, persisted) = self.db.submit_command(&NewCommandRecord {
            thread_id: params.thread_id.clone(),
            command_type: params.command_type.clone(),
            requested_by: params.requested_by.clone(),
            params: params.params.clone(),
        })?;
        self.events.publish_persisted_batch(&persisted);
        Ok(command)
    }

    pub fn claim(&self, params: &CommandClaimParams) -> Result<CommandClaimResult> {
        let _timeout_ms = params.timeout_ms.unwrap_or(0);
        let (commands, persisted) = self.db.claim_commands(&params.thread_id)?;
        self.events.publish_persisted_batch(&persisted);
        Ok(CommandClaimResult { commands })
    }

    pub fn complete(&self, params: &CommandCompleteParams) -> Result<CommandRecord> {
        match params.status.as_str() {
            "completed" | "rejected" => {}
            other => bail!("invalid command completion status: {other}"),
        }

        let (command, persisted) =
            self.db
                .complete_command(params.command_id, &params.status, params.result.as_ref())?;
        self.events.publish_persisted_batch(&persisted);
        Ok(command)
    }
}

fn validate_command_type(command_type: &str) -> Result<()> {
    match command_type {
        "cancel" | "kill" | "interrupt" | "continue" => Ok(()),
        other => bail!("invalid command_type: {other}"),
    }
}
