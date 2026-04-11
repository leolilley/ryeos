use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::identity::NodeIdentity;
use crate::state::AppState;

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

pub async fn status(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(state.status()).unwrap_or_else(|_| json!({ "status": "error" })))
}

pub async fn public_key(
    State(state): State<AppState>,
) -> Result<Json<Value>, (axum::http::StatusCode, Json<Value>)> {
    let identity_path = state
        .config
        .state_dir
        .join("identity")
        .join("public-identity.json");
    let doc = NodeIdentity::load_public_identity(&identity_path).map_err(|err| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
    })?;
    Ok(Json(
        serde_json::to_value(doc).unwrap_or_else(|_| json!({ "error": "encode_failed" })),
    ))
}
