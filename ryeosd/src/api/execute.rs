use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::task;

use crate::bridge::ExecutionCompletion;
use crate::services::thread_lifecycle::{ExecuteBudgetRequest, ThreadFinalizeParams};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ExecuteRequest {
    pub item_ref: String,
    #[serde(default)]
    pub parameters: Value,
    #[serde(default = "default_launch_mode")]
    pub launch_mode: String,
    #[serde(default)]
    pub target_site_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub budget: Option<ExecuteBudgetRequest>,
}

fn default_launch_mode() -> String {
    "inline".to_string()
}

pub async fn execute(
    State(state): State<AppState>,
    Json(request): Json<ExecuteRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut resolved = state
        .threads
        .resolve_root_execution(
            std::env::current_dir().map_err(|err| internal_error(err.into()))?,
            &request.item_ref,
            &request.launch_mode,
            request.parameters.clone(),
            Some(state.identity.principal_id()),
            request.model.clone(),
            request.budget.clone(),
        )
        .map_err(invalid_request)?;

    // Remote site passthrough: tag the thread with the target site metadata
    // while still executing locally. Real HTTP forwarding will be added when
    // ryeosd replaces ryeos-node in production.
    if let Some(target) = &request.target_site_id {
        resolved.current_site_id = target.clone();
    }

    if !state.threads.kind_profiles().is_root_executable(&resolved.kind) {
        return Err(not_implemented(&format!(
            "kind '{}' is not root executable",
            resolved.kind
        )));
    }

    let created_thread = state
        .threads
        .create_root_thread(&resolved)
        .map_err(internal_error)?;
    let running_thread = state
        .threads
        .mark_running(&created_thread.thread_id)
        .map_err(internal_error)?;

    let mut execution_request = state.threads.build_execution_request(
        &running_thread,
        &resolved.project_path,
        resolved.parameters.clone(),
        None,
    );
    execution_request.runtime.socket_path = state.config.uds_path.display().to_string();

    if request.launch_mode == "detached" {
        let bg_state = state.clone();
        let bg_thread_id = running_thread.thread_id.clone();
        let bg_bridge = state.bridge.clone();
        tokio::spawn(async move {
            let result =
                task::spawn_blocking(move || bg_bridge.execute_item(&execution_request)).await;
            match result {
                Ok(Ok(completion)) => {
                    let _ = finalize_completion(&bg_state, &bg_thread_id, completion);
                }
                Ok(Err(err)) => {
                    let _ = bg_state.threads.finalize_thread(&ThreadFinalizeParams {
                        thread_id: bg_thread_id,
                        status: "failed".to_string(),
                        outcome_code: Some("bridge_error".to_string()),
                        result: None,
                        error: Some(json!({ "message": err.to_string() })),
                        metadata: None,
                        artifacts: Vec::new(),
                        final_cost: None,
                        summary_json: None,
                    });
                }
                Err(join_err) => {
                    let _ = bg_state.threads.finalize_thread(&ThreadFinalizeParams {
                        thread_id: bg_thread_id,
                        status: "failed".to_string(),
                        outcome_code: Some("task_panic".to_string()),
                        result: None,
                        error: Some(json!({ "message": join_err.to_string() })),
                        metadata: None,
                        artifacts: Vec::new(),
                        final_cost: None,
                        summary_json: None,
                    });
                }
            }
        });

        return Ok(Json(json!({
            "thread": running_thread,
            "detached": true,
        })));
    }

    // Inline execution: await completion and return full result
    let bridge = state.bridge.clone();
    let completion = task::spawn_blocking(move || bridge.execute_item(&execution_request))
        .await
        .map_err(|err| internal_error(err.into()))?
        .map_err(|err| {
            let _ = state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: running_thread.thread_id.clone(),
                status: "failed".to_string(),
                outcome_code: Some("bridge_error".to_string()),
                result: None,
                error: Some(json!({ "message": err.to_string() })),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            });
            internal_error(err)
        })?;

    let finalized_thread = finalize_completion(&state, &running_thread.thread_id, completion)
        .map_err(internal_error)?;
    let result = state
        .threads
        .build_execute_result(&finalized_thread.thread_id)
        .map_err(internal_error)?;

    Ok(Json(json!({
        "thread": finalized_thread,
        "result": result,
    })))
}

fn finalize_completion(
    state: &AppState,
    thread_id: &str,
    completion: ExecutionCompletion,
) -> anyhow::Result<crate::db::ThreadDetail> {
    match state
        .threads
        .finalize_from_completion(thread_id, &completion)
    {
        Ok(thread) => Ok(thread),
        Err(err) => {
            let _ = state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: thread_id.to_string(),
                status: "failed".to_string(),
                outcome_code: Some("invalid_completion".to_string()),
                result: None,
                error: Some(json!({
                    "message": err.to_string(),
                    "completion": completion,
                })),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            });
            Err(err)
        }
    }
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

fn not_implemented(message: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": message })),
    )
}
