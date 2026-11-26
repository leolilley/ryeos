//! `bundle.list` — enumerate registered bundles from the node-config snapshot,
//! and the bundles staged for a later start.
//!
//! `bundles` holds only installed (registered) bundles. `staged` holds the
//! stages in the node's staging area, each with status `staged`: inert bytes,
//! not registered and not loaded. An operator sees the stage's detail (digest,
//! source, requester, what the bundle declares); any other caller sees the name
//! and the status only. A stage whose record does not verify is not listed.
//! `apply_refused` is a later status of a stage and has its place here.

use std::sync::Arc;

use anyhow::Result;
use ryeos_engine::trust::{SignerRole, SignerStatus};
use ryeos_node::bundle_staging::{STATUS_STAGED, StagedBundle, StagingArea};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

/// Whether the caller is the node's own active operator, not a forwarded
/// request. Read only: a caller that is not one is simply not shown detail.
fn caller_is_operator(ctx: &HandlerContext, state: &AppState) -> bool {
    if ctx.authenticated_origin_site_id.is_some() || ctx.delegated_via.is_some() {
        return false;
    }
    let fingerprint = ctx
        .fingerprint
        .strip_prefix("fp:")
        .unwrap_or(&ctx.fingerprint);
    state
        .engine
        .trust_store
        .signer(fingerprint)
        .is_some_and(|record| {
            record.role == SignerRole::Operator && record.status == SignerStatus::Active
        })
}

/// One stage as the caller may see it.
fn staged_entry(stage: &StagedBundle, operator: bool) -> Value {
    let mut entry = serde_json::json!({
        "name": stage.record.bundle_name,
        "status": STATUS_STAGED,
    });
    if operator && let Some(object) = entry.as_object_mut() {
        object.insert(
            "tree_digest".into(),
            serde_json::json!(stage.record.tree_digest),
        );
        object.insert("source".into(), serde_json::json!(stage.record.source));
        object.insert(
            "requested_by".into(),
            serde_json::json!(stage.record.requested_by),
        );
        object.insert("declared".into(), serde_json::json!(stage.record.declared));
        object.insert(
            "file_count".into(),
            serde_json::json!(stage.record.file_count),
        );
        object.insert(
            "total_bytes".into(),
            serde_json::json!(stage.record.total_bytes),
        );
        object.insert(
            "staged_at".into(),
            serde_json::json!(stage.record.staged_at),
        );
    }
    entry
}

pub async fn handle(_req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let operator = caller_is_operator(&ctx, &state);
    let mut bundles = Vec::new();
    for record in &state.node_config.bundles {
        bundles.push(serde_json::json!({
            "name": record.name,
            "path": record.path.display().to_string(),
            "status": "installed",
        }));
    }
    let node_key = state.identity.signing_key().verifying_key();
    let staged: Vec<Value> =
        match StagingArea::open_if_present(&state.config.app_root).map_err(anyhow::Error::new)? {
            Some(area) => area
                .list(&node_key)
                .map_err(anyhow::Error::new)?
                .iter()
                .map(|stage| staged_entry(stage, operator))
                .collect(),
            None => Vec::new(),
        };
    Ok(serde_json::json!({ "bundles": bundles, "staged": staged }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/list",
    endpoint: "bundle.list",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            handle(req, ctx, state).await
        })
    },
};
