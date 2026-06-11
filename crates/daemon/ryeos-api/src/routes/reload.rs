use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::api_state::ApiState;

pub async fn handle_routes_reload(state: &ApiState) -> Result<Value> {
    let new_table =
        super::build_route_table_from_snapshot(&state.app.node_config).map_err(|errors| {
            let msgs: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
            anyhow::anyhow!(
                "failed to build route table during reload: {}",
                msgs.join("; ")
            )
        })?;

    let fingerprint = new_table.fingerprint.clone();
    let route_count = new_table.all.len();
    let diagnostic_entries = super::route_diagnostic_entries(&new_table);

    state.route_table.store(Arc::new(new_table));

    // Keep the `service:system/routes` snapshot in step with the live
    // table. Absent only in embedded test states.
    if let Some(diags) = state
        .app
        .extensions
        .get::<ryeos_app::route_diagnostics::RouteDiagnostics>()
    {
        diags.publish(fingerprint.clone(), diagnostic_entries);
    }

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
