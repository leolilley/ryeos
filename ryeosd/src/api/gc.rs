use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::gc::{self, GCParams};
use crate::policy;
use crate::state::AppState;

/// POST /gc — trigger a garbage collection run.
pub async fn run_gc(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "admin")?;

    let body_bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;

    let params: GCParams = if body_bytes.is_empty() {
        GCParams::default()
    } else {
        serde_json::from_slice(&body_bytes)
            .map_err(|err| (StatusCode::BAD_REQUEST, Json(json!({ "error": err.to_string() }))))?
    };

    let node_id = state.identity.fingerprint().to_string();

    let result = gc::run_gc(
        state.cas_store(),
        state.refs_store(),
        &state.config.cas_root,
        &node_id,
        &params,
    )
    .map_err(policy::internal_error)?;

    Ok(Json(serde_json::to_value(&result).unwrap()))
}
