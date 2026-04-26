use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

pub async fn health(State(state): State<AppState>) -> Json<Value> {
    let status = if state.catalog_health.missing_services.is_empty() {
        "healthy"
    } else {
        // Should never reach here — V5.2 fail-closed self-check bails on startup.
        "degraded"
    };

    Json(json!({
        "status": status,
        "operational_services": state.catalog_health.status,
        "missing_services": state.catalog_health.missing_services,
    }))
}
