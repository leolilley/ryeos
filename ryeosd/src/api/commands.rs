use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::process::{is_daemon_pgid, kill_process_group};
use crate::services::command_service::CommandSubmitParams;
use crate::services::thread_lifecycle::ThreadFinalizeParams;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CommandSubmitRequest {
    pub command_type: String,
    #[serde(default)]
    pub params: Option<Value>,
}

pub async fn submit_command(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(request): Json<CommandSubmitRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let command_type = request.command_type.clone();

    // cancel/interrupt/continue are not yet consumed by runtimes — reject early
    if matches!(command_type.as_str(), "cancel" | "interrupt" | "continue") {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "error": format!(
                    "'{}' command delivery is not yet implemented; \
                     runtime does not poll for commands",
                    command_type
                )
            })),
        ));
    }

    let command = state
        .commands
        .submit(&CommandSubmitParams {
            thread_id: thread_id.clone(),
            command_type: command_type.clone(),
            requested_by: Some(state.identity.principal_id()),
            params: request.params,
        })
        .map_err(invalid_request)?;

    // For kill commands, the daemon acts immediately — don't wait for
    // the runtime to claim the command.
    if command_type == "kill" {
        let kill_result =
            tokio::task::spawn_blocking(move || execute_kill(&state, &thread_id))
                .await
                .map_err(|err| internal_error(err.into()))?;
        if let Err(err) = kill_result {
            return Err(internal_error(err));
        }
    }

    Ok(Json(json!({ "command": command })))
}

fn execute_kill(state: &AppState, thread_id: &str) -> anyhow::Result<()> {
    let thread = state
        .threads
        .get_thread(thread_id)?
        .ok_or_else(|| anyhow::anyhow!("thread not found: {thread_id}"))?;

    let pgid = thread
        .runtime
        .pgid
        .ok_or_else(|| anyhow::anyhow!("thread has no process group attached"))?;

    if is_daemon_pgid(pgid) {
        anyhow::bail!(
            "cannot kill thread {thread_id}: PGID {pgid} matches daemon process group"
        );
    }

    let grace = std::time::Duration::from_secs(5);
    let result = kill_process_group(pgid, grace);

    let terminal_status = if result.success { "killed" } else { "failed" };

    state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: terminal_status.to_string(),
        outcome_code: Some(format!("kill_{}", result.method)),
        result: None,
        error: Some(json!({
            "message": format!("process group {} killed (method={})", pgid, result.method),
            "pgid": pgid,
        })),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    })?;

    Ok(())
}

fn invalid_request(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": err.to_string() })),
    )
}

fn internal_error(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": err.to_string() })),
    )
}
