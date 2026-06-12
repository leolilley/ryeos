//! `system.routes` — operator diagnostics for the live node:
//! loaded route table (ids, methods, paths, source files) and
//! registered bundles (name, installed path, whether the path exists).
//!
//! Answers, without source-level debugging:
//! * is a specific route path/method actually loaded?
//! * which bundle registrations exist and do their paths exist on disk?
//!
//! The route snapshot comes from `AppState::extensions` (published by
//! the composition root at boot and on reload). If the extension is
//! absent — embedded test states that never build a route table — the
//! response says so explicitly instead of reporting zero routes.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::route_diagnostics::RouteDiagnostics;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    /// Optional substring filter on the route path pattern.
    pub path: Option<String>,
    /// Optional exact (case-insensitive) method filter.
    pub method: Option<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let bundles: Vec<Value> = state
        .node_config
        .bundles
        .iter()
        .map(|b| {
            serde_json::json!({
                "name": b.name,
                "path": b.path.display().to_string(),
                "path_exists": b.path.is_dir(),
                "source_file": b.source_file.display().to_string(),
            })
        })
        .collect();

    let (routes_available, fingerprint, routes) = match state.extensions.get::<RouteDiagnostics>() {
        Some(diags) => {
            let entries = diags.entries();
            let method_filter = req.method.as_deref().map(str::to_ascii_uppercase);
            let filtered: Vec<Value> = entries
                .iter()
                .filter(|e| req.path.as_deref().is_none_or(|p| e.path.contains(p)))
                .filter(|e| {
                    method_filter
                        .as_deref()
                        .is_none_or(|m| e.methods.iter().any(|rm| rm == m))
                })
                .map(|e| serde_json::to_value(e).expect("RouteDiagnosticEntry serializes"))
                .collect();
            (true, diags.fingerprint().to_string(), filtered)
        }
        None => (false, String::new(), Vec::new()),
    };

    Ok(serde_json::json!({
        "routes_available": routes_available,
        "route_fingerprint": fingerprint,
        "route_count": routes.len(),
        "routes": routes,
        "bundles": bundles,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:system/routes",
    endpoint: "system.routes",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.system/routes"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                serde_json::from_value(params)?
            };
            handle(req, state).await
        })
    },
};
