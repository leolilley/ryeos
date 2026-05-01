use std::sync::Arc;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::kind_profiles::KindProfileRegistry;
use crate::state_store::{CommandRecord, NewCommandRecord, StateStore};
use crate::services::event_store::EventStoreService;

#[derive(Debug, Clone)]
pub struct CommandService {
    state_store: Arc<StateStore>,
    kind_profiles: Arc<KindProfileRegistry>,
    _events: Arc<EventStoreService>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSubmitParams {
    pub thread_id: String,
    pub command_type: String,
    #[serde(default)]
    pub requested_by: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct CommandCompleteParams {
    pub command_id: i64,
    pub status: String,
    #[serde(default)]
    pub result: Option<Value>,
}

impl CommandService {
    pub fn new(
        state_store: Arc<StateStore>,
        kind_profiles: Arc<KindProfileRegistry>,
        _events: Arc<EventStoreService>,
    ) -> Self {
        Self { state_store, kind_profiles, _events }
    }

    #[tracing::instrument(
        level = "debug",
        name = "command:submit",
        skip(self, params),
        fields(
            thread_id = %params.thread_id,
            command_type = %params.command_type,
        )
    )]
    pub fn submit(&self, params: &CommandSubmitParams) -> Result<CommandRecord> {
        validate_command_type(&params.command_type)?;

        let thread = self
            .state_store
            .get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread not found: {}", params.thread_id))?;

        let allowed = self.kind_profiles.allowed_actions(
            &thread.kind,
            &thread.status,
            thread.runtime.pgid.is_some(),
        );
        if !allowed.iter().any(|action| action == &params.command_type) {
            bail!(
                "command {} is not allowed for thread {} in status {}",
                params.command_type,
                params.thread_id,
                thread.status
            );
        }

        let command = self.state_store.submit_command(&NewCommandRecord {
            thread_id: params.thread_id.clone(),
            command_type: params.command_type.clone(),
            requested_by: params.requested_by.clone(),
            params: params.params.clone(),
        })?;
        Ok(command)
    }

    #[tracing::instrument(
        level = "debug",
        name = "command:claim",
        skip(self, params),
        fields(thread_id = %params.thread_id)
    )]
    pub fn claim(&self, params: &CommandClaimParams) -> Result<CommandClaimResult> {
        let _timeout_ms = params.timeout_ms.unwrap_or(0);
        let commands = self.state_store.claim_commands(&params.thread_id)?;
        Ok(CommandClaimResult { commands })
    }

    #[tracing::instrument(
        level = "debug",
        name = "command:complete",
        skip(self, params),
        fields(
            command_id = params.command_id,
            status = %params.status,
        )
    )]
    pub fn complete(&self, params: &CommandCompleteParams) -> Result<CommandRecord> {
        match params.status.as_str() {
            "completed" | "rejected" => {}
            other => bail!("invalid command completion status: {other}"),
        }

        let command =
            self.state_store
                .complete_command(params.command_id, &params.status, params.result.as_ref())?;
        Ok(command)
    }
}

fn validate_command_type(command_type: &str) -> Result<()> {
    match command_type {
        "cancel" | "kill" | "interrupt" | "continue" => Ok(()),
        other => bail!("invalid command_type: {other}"),
    }
}
