use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::task;

use rye_engine::contracts::ExecutionCompletion;

use crate::policy;
use crate::services::thread_lifecycle::{
    self, ThreadAttachProcessParams, ThreadFinalizeParams,
};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ExecuteRequest {
    pub item_ref: String,
    /// Project root path for three-tier resolution. Required when the caller
    /// (e.g. MCP server) runs in a different cwd than the user's project.
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub parameters: Value,
    #[serde(default = "default_launch_mode")]
    pub launch_mode: String,
    #[serde(default)]
    pub target_site_id: Option<String>,
}

fn default_launch_mode() -> String {
    "inline".to_string()
}

pub async fn execute(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_principal_id = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "execute")?;

    // Deserialize the body
    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;
    let request: ExecuteRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| invalid_request(err.into()))?;

    let site_id = state.threads.site_id();
    let project_path = match &request.project_path {
        Some(p) => std::path::PathBuf::from(p),
        None => std::env::current_dir().map_err(|err| policy::internal_error(err.into()))?,
    };
    let mut resolved = thread_lifecycle::resolve_root_execution(
            &state.engine,
            site_id,
            &project_path,
            &request.item_ref,
            &request.launch_mode,
            request.parameters.clone(),
            Some(caller_principal_id),
            caller_scopes,
        )
        .map_err(invalid_request)?;

    if let Some(target) = request.target_site_id {
        resolved.target_site_id = Some(target);
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
        .map_err(policy::internal_error)?;
    let running_thread = state
        .threads
        .mark_running(&created_thread.thread_id)
        .map_err(policy::internal_error)?;

    if request.launch_mode == "detached" {
        let bg_state = state.clone();
        let bg_thread_id = running_thread.thread_id.clone();
        let bg_chain_root_id = running_thread.chain_root_id.clone();
        let bg_resolved = resolved.clone();
        let bg_engine = state.engine.clone();
        tokio::spawn(async move {
            let thread_id_for_spawn = bg_thread_id.clone();
            let chain_root_for_spawn = bg_chain_root_id.clone();
            let resolved_for_spawn = bg_resolved.clone();
            let engine_for_spawn = bg_engine.clone();

            // Spawn phase (blocking)
            let spawn_result = task::spawn_blocking(move || {
                thread_lifecycle::spawn_item(
                    &engine_for_spawn,
                    &resolved_for_spawn,
                    &thread_id_for_spawn,
                    &chain_root_for_spawn,
                )
            }).await;

            match spawn_result {
                Ok(Ok(spawned)) => {
                    // Attach pid/pgid
                    let _ = bg_state.threads.attach_process(
                        &ThreadAttachProcessParams {
                            thread_id: bg_thread_id.clone(),
                            pid: spawned.pid as i64,
                            pgid: spawned.pgid,
                            metadata: None,
                        },
                    );

                    // Wait phase (blocking)
                    let wait_result = task::spawn_blocking(move || spawned.wait()).await;
                    match wait_result {
                        Ok(completion) => {
                            let _ = finalize_completion(&bg_state, &bg_thread_id, completion);
                        }
                        Err(join_err) => {
                            tracing::error!(error = %join_err, "task panic during detached wait");
                            let _ = bg_state.threads.finalize_thread(&ThreadFinalizeParams {
                                thread_id: bg_thread_id,
                                status: "failed".to_string(),
                                outcome_code: Some("task_panic".to_string()),
                                result: None,
                                error: Some(json!({ "code": "task_panic" })),
                                metadata: None,
                                artifacts: Vec::new(),
                                final_cost: None,
                                summary_json: None,
                            });
                        }
                    }
                }
                Ok(Err(err)) => {
                    tracing::error!(error = %err, "engine error during detached spawn");
                    let _ = bg_state.threads.finalize_thread(&ThreadFinalizeParams {
                        thread_id: bg_thread_id,
                        status: "failed".to_string(),
                        outcome_code: Some("engine_error".to_string()),
                        result: None,
                        error: Some(json!({ "code": "engine_error" })),
                        metadata: None,
                        artifacts: Vec::new(),
                        final_cost: None,
                        summary_json: None,
                    });
                }
                Err(join_err) => {
                    tracing::error!(error = %join_err, "task panic during detached spawn");
                    let _ = bg_state.threads.finalize_thread(&ThreadFinalizeParams {
                        thread_id: bg_thread_id,
                        status: "failed".to_string(),
                        outcome_code: Some("task_panic".to_string()),
                        result: None,
                        error: Some(json!({ "code": "task_panic" })),
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

    // Inline execution: spawn → attach process → wait
    let engine = state.engine.clone();
    let thread_id_for_spawn = running_thread.thread_id.clone();
    let chain_root_id_for_spawn = running_thread.chain_root_id.clone();
    let spawned = task::spawn_blocking(move || {
        thread_lifecycle::spawn_item(&engine, &resolved, &thread_id_for_spawn, &chain_root_id_for_spawn)
    })
    .await
    .map_err(|err| policy::internal_error(err.into()))?
    .map_err(|err| {
        tracing::error!(error = %err, "engine error during inline spawn");
        let _ = state.threads.finalize_thread(&ThreadFinalizeParams {
            thread_id: running_thread.thread_id.clone(),
            status: "failed".to_string(),
            outcome_code: Some("engine_error".to_string()),
            result: None,
            error: Some(json!({ "code": "engine_error" })),
            metadata: None,
            artifacts: Vec::new(),
            final_cost: None,
            summary_json: None,
        });
        policy::internal_error(err)
    })?;

    // Persist pid/pgid so kill/shutdown can find this process
    let _ = state.threads.attach_process(&ThreadAttachProcessParams {
        thread_id: running_thread.thread_id.clone(),
        pid: spawned.pid as i64,
        pgid: spawned.pgid,
        metadata: None,
    });

    // Now wait for completion (blocking)
    let completion = task::spawn_blocking(move || spawned.wait())
        .await
        .map_err(|err| policy::internal_error(err.into()))?;

    let finalized_thread = finalize_completion(&state, &running_thread.thread_id, completion)
        .map_err(policy::internal_error)?;
    let result = state
        .threads
        .build_execute_result(&finalized_thread.thread_id)
        .map_err(policy::internal_error)?;

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
            tracing::error!(error = %err, "invalid completion during finalization");
            let _ = state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: thread_id.to_string(),
                status: "failed".to_string(),
                outcome_code: Some("invalid_completion".to_string()),
                result: None,
                error: Some(json!({ "code": "invalid_completion" })),
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

fn not_implemented(message: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": message })),
    )
}
