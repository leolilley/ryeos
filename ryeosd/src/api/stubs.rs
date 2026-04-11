use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

pub async fn not_implemented() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": "endpoint not implemented in ryeosd scaffold" })),
    )
}
