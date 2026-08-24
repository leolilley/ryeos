use std::io::Read as _;

use anyhow::{Context, Result, anyhow, bail};
use ryeos_runtime::callback::{
    DedicatedSessionCommandRequest, DedicatedSessionStartRequest, DedicatedSessionTerminateRequest,
    RuntimeCallbackAPI,
};
use ryeos_runtime::callback_uds::UdsRuntimeClient;
#[cfg(test)]
use ryeos_runtime::envelope::RuntimeResultStatus;
use ryeos_runtime::envelope::{LaunchEnvelope, RuntimeResult};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inputs {
    credential_profile_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerExecutionConfig {
    worker_ref: String,
    required_credential_state: String,
    route_set: String,
    allowed_effect_classes: Vec<String>,
    credential_home_env: String,
    workspace_env: String,
    require_pinned_cow: bool,
    required_terminal_publication: String,
    max_lifetime_seconds: u64,
    recover_remote_session: bool,
}

fn main() {
    let result = run();
    match result {
        Ok(result) => println!("{}", serde_json::to_string(&result).unwrap()),
        Err(error) => {
            eprintln!("ryeos-worker-execution-runtime: {error:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<RuntimeResult> {
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;
    let envelope: LaunchEnvelope =
        serde_json::from_slice(&bytes).context("decode launch envelope")?;
    if envelope.schema_version()
        != ryeos_engine::launch_envelope_types::MANAGED_LAUNCH_ENVELOPE_SCHEMA_VERSION
    {
        bail!("unsupported managed launch envelope schema");
    }
    let inputs: Inputs = serde_json::from_value(envelope.request.inputs.clone())
        .context("decode worker execution inputs")?;
    let config: WorkerExecutionConfig = serde_json::from_value(
        envelope
            .runtime_data
            .get("worker_execution")
            .cloned()
            .ok_or_else(|| anyhow!("worker execution runtime data is absent"))?,
    )
    .context("decode admitted worker execution config")?;
    let callback_token = std::env::var("RYEOSD_CALLBACK_TOKEN")
        .context("RYEOSD_CALLBACK_TOKEN must be set by daemon")?;
    let thread_auth_token = std::env::var("RYEOSD_THREAD_AUTH_TOKEN")
        .context("RYEOSD_THREAD_AUTH_TOKEN must be set by daemon")?;
    let client = UdsRuntimeClient::new(
        envelope.callback.socket_path.clone(),
        callback_token,
        thread_auth_token,
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_session(client, envelope.thread_id, inputs, config))
}

async fn run_session(
    client: UdsRuntimeClient,
    thread_id: String,
    inputs: Inputs,
    config: WorkerExecutionConfig,
) -> Result<RuntimeResult> {
    if config.worker_ref.len() > 256
        || !config.worker_ref.starts_with("worker:")
        || config.max_lifetime_seconds == 0
        || config.max_lifetime_seconds > 603_600
    {
        bail!("admitted worker execution config is outside runtime bounds");
    }
    let max_lifetime = std::time::Duration::from_secs(config.max_lifetime_seconds);
    let deadline = tokio::time::Instant::now() + max_lifetime;
    client
        .mark_running(&thread_id)
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    let mut started = client
        .start_dedicated_session(DedicatedSessionStartRequest {
            thread_id: thread_id.clone(),
            dependency_ref: config.worker_ref.clone(),
            credential_profile_id: inputs.credential_profile_id,
            required_credential_state: config.required_credential_state.clone(),
            route_set: config.route_set.clone(),
            allowed_effect_classes: config.allowed_effect_classes.clone(),
            credential_home_env: config.credential_home_env.clone(),
            workspace_env: config.workspace_env.clone(),
            require_pinned_cow: config.require_pinned_cow,
            required_terminal_publication: config.required_terminal_publication.clone(),
        })
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    if started.get("state").and_then(Value::as_str) == Some("recovering") {
        if !config.recover_remote_session {
            let terminal = client
                .terminate_dedicated_session(DedicatedSessionTerminateRequest {
                    thread_id: thread_id.clone(),
                    reason: "cancelled".to_string(),
                })
                .await
                .map_err(|error| anyhow!(error.to_string()))?;
            return Ok(terminal_result(thread_id, terminal));
        }
        let remote_thread_id = started
            .get("remote_thread_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("recovering dedicated session has no retained remote thread"))?;
        let worker_boot_epoch = started
            .get("worker_boot_epoch")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("recovering dedicated session has no worker boot epoch"))?;
        client
            .dedicated_session_command(DedicatedSessionCommandRequest {
                thread_id: thread_id.clone(),
                idempotency_key: format!("recovery:{worker_boot_epoch}:{remote_thread_id}"),
                command_kind: "reattach".to_string(),
                payload: json!({
                    "route_id":"session.resume",
                    "payload":{"threadId":remote_thread_id},
                }),
            })
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        client
            .dedicated_session_command(DedicatedSessionCommandRequest {
                thread_id: thread_id.clone(),
                idempotency_key: format!("recovery-status:{worker_boot_epoch}:{remote_thread_id}"),
                command_kind: "reattach".to_string(),
                payload: json!({
                    "route_id":"session.read",
                    "payload":{"threadId":remote_thread_id},
                }),
            })
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        started = client
            .dedicated_session_status(&thread_id)
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        if started.get("state").and_then(Value::as_str) != Some("idle") {
            bail!(
                "remote session was reattached but its pinned status did not prove an idle turn boundary"
            );
        }
    }
    loop {
        let status = started
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(status, "terminal" | "freezing") {
            return Ok(terminal_result(thread_id, started));
        }
        if tokio::time::Instant::now() >= deadline {
            let terminal = client
                .terminate_dedicated_session(DedicatedSessionTerminateRequest {
                    thread_id: thread_id.clone(),
                    reason: "cancelled".to_string(),
                })
                .await
                .map_err(|error| anyhow!(error.to_string()))?;
            return Ok(terminal_result(thread_id, terminal));
        }
        let observed_updated_at_ms = started
            .get("updated_at_ms")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("dedicated session projection has no update sequence"))?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let wait = remaining.min(std::time::Duration::from_secs(300));
        let current = client
            .wait_dedicated_session(ryeos_runtime::callback::DedicatedSessionWaitRequest {
                thread_id: thread_id.clone(),
                observed_updated_at_ms,
                timeout_ms: u64::try_from(wait.as_millis()).unwrap_or(300_000).max(1),
            })
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        if matches!(
            current.get("state").and_then(Value::as_str),
            Some("terminal" | "freezing")
        ) {
            return Ok(terminal_result(thread_id, current));
        }
        started = current;
    }
}

fn terminal_result(thread_id: String, session: Value) -> RuntimeResult {
    ryeos_runtime::envelope::dedicated_session_terminal_result(thread_id, session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_projection_preserves_outcome_instead_of_reporting_success() {
        let completed = terminal_result(
            "completed".to_owned(),
            json!({"terminal_reason":"completed"}),
        );
        assert_eq!(completed.status, RuntimeResultStatus::Completed);
        assert!(completed.success);

        let cancelled = terminal_result(
            "cancelled".to_owned(),
            json!({"terminal_reason":"cancelled"}),
        );
        assert_eq!(cancelled.status, RuntimeResultStatus::Cancelled);
        assert!(!cancelled.success);

        let revoked = terminal_result(
            "revoked".to_owned(),
            json!({"terminal_reason":"credential_revoked"}),
        );
        assert_eq!(revoked.status, RuntimeResultStatus::Failed);
        assert!(!revoked.success);
    }
}
