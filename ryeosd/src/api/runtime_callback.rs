use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct RuntimeCallbackRequest {
    pub callback_token: String,
    pub thread_id: String,
    pub project_path: String,
    #[serde(flatten)]
    pub extra: Value,
}

pub async fn runtime_callback(
    Path(method): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<RuntimeCallbackRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let project_path =
        crate::execution::project_source::normalize_project_path(&request.project_path);

    state
        .callback_tokens
        .validate(&request.callback_token, &request.thread_id, &project_path)
        .map_err(|e| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "status": "error", "error": e.to_string() })),
            )
        })?;

    let method_str = format!("runtime.{method}");
    let params = build_params(&method_str, &request);

    let result = tokio::task::spawn_blocking(move || {
        crate::uds::server::dispatch_runtime_method(&method_str, &params, &state)
    })
    .await;

    match result {
        Ok(Ok(value)) => Ok(Json(value)),
        Ok(Err(err)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err.to_string() })),
        )),
        Err(join_err) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "status": "error",
                "error": format!("task panic: {join_err}"),
            })),
        )),
    }
}

fn build_params(_method: &str, request: &RuntimeCallbackRequest) -> Value {
    let mut params = request.extra.clone();
    if let Some(map) = params.as_object_mut() {
        map.insert(
            "callback_token".to_string(),
            json!(request.callback_token),
        );
        map.insert("thread_id".to_string(), json!(request.thread_id));
        map.insert("project_path".to_string(), json!(request.project_path));
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_params_includes_required_fields() {
        let request = RuntimeCallbackRequest {
            callback_token: "cbt-test".to_string(),
            thread_id: "T-1".to_string(),
            project_path: "/project".to_string(),
            extra: json!({"action": {"primary": "execute"}}),
        };
        let params = build_params("runtime.dispatch_action", &request);
        assert_eq!(params["callback_token"], "cbt-test");
        assert_eq!(params["thread_id"], "T-1");
        assert_eq!(params["project_path"], "/project");
        assert_eq!(params["action"]["primary"], "execute");
    }
}
