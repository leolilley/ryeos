use std::sync::Arc;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event_store_service::EventStoreService;
use crate::kind_profiles::KindProfileRegistry;
use crate::runtime_db::{
    validate_command_type, MAX_COMMAND_CLAIM_ITEMS, MAX_COMMAND_CLAIM_RESPONSE_BYTES,
    MAX_COMMAND_PARAMS_BYTES, MAX_COMMAND_REQUESTED_BY_BYTES, MAX_COMMAND_RESULT_BYTES,
};
use crate::state_store::{CommandRecord, NewCommandRecord, StateStore};

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
        Self {
            state_store,
            kind_profiles,
            _events,
        }
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
        validate_optional_json_size(
            "command params",
            params.params.as_ref(),
            MAX_COMMAND_PARAMS_BYTES,
        )?;
        if params
            .requested_by
            .as_ref()
            .is_some_and(|requested_by| requested_by.len() > MAX_COMMAND_REQUESTED_BY_BYTES)
        {
            bail!("command requested_by exceeds the {MAX_COMMAND_REQUESTED_BY_BYTES}-byte maximum");
        }

        let thread = self
            .state_store
            .get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread not found: {}", params.thread_id))?;

        // Reject commands that are not valid for the durable thread state
        // before consulting live launch ownership. A terminal recorded
        // in-process thread deliberately retains its launch driver after its
        // live owner is gone; treating that settled state as contradictory
        // authority would hide the authoritative terminal command gate.
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

        if matches!(params.command_type.as_str(), "cancel" | "kill") {
            if let Some(refusal) = self
                .state_store
                .in_process_handler_stop_refusal(&params.thread_id)?
            {
                bail!("{}", refusal.message(&params.thread_id));
            }
        }

        // AUTHZ NOTE: per-thread ownership is enforced upstream in
        // handlers/commands_submit.rs (`ctx.require_owner`, commit 4510c4bc) —
        // this service runs only after the caller is confirmed the thread's
        // owner. This layer gates on capability + allowed_actions(kind,status).
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
        let commands = self.state_store.claim_commands(
            &params.thread_id,
            MAX_COMMAND_CLAIM_ITEMS,
            MAX_COMMAND_CLAIM_RESPONSE_BYTES,
        )?;
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
        validate_optional_json_size(
            "command result",
            params.result.as_ref(),
            MAX_COMMAND_RESULT_BYTES,
        )?;

        let command = self.state_store.complete_command(
            params.command_id,
            &params.status,
            params.result.as_ref(),
        )?;
        Ok(command)
    }
}

fn validate_optional_json_size(label: &str, value: Option<&Value>, maximum: usize) -> Result<()> {
    let bytes = value
        .map(serde_json::to_vec)
        .transpose()?
        .map_or(0, |v| v.len());
    if bytes > maximum {
        bail!("{label} is {bytes} bytes; maximum is {maximum}");
    }
    Ok(())
}
