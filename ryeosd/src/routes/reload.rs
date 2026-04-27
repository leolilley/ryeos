use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::state::AppState;

pub async fn handle_routes_reload(
    state: &AppState,
) -> Result<Value> {
    let new_table = super::build_route_table_from_snapshot(&state.node_config)
        .map_err(|errors| {
            let msgs: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
            anyhow::anyhow!("failed to build route table during reload: {}", msgs.join("; "))
        })?;

    let fingerprint = new_table.fingerprint.clone();
    let route_count = new_table.all.len();

    state.route_table.store(Arc::new(new_table));

    tracing::info!(
        fingerprint = %fingerprint,
        route_count = route_count,
        "route table reloaded"
    );

    Ok(json!({
        "fingerprint": fingerprint,
        "route_count": route_count,
    }))
}
