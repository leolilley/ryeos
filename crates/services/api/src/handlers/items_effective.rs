//! `items.effective` — resolve, verify, compose, and return an effective item.
//!
//! Works for any item kind (executable or not). The engine owns the
//! resolution/trust/composition semantics; this handler is intentionally
//! only a typed service wrapper.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::EffectiveItemRequest;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Canonical item ref to resolve, e.g. "surface:ryeos/cockpit/base".
    pub canonical_ref: String,

    /// Optional project path for project-space resolution.
    /// When absent, only system and user spaces are searched.
    #[serde(default)]
    pub project_path: Option<String>,

    /// Optional guardrail for clients that know which kind they expect.
    #[serde(default)]
    pub expected_kind: Option<String>,
}

pub async fn handle(req: Request, _ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let item_ref = CanonicalRef::parse(&req.canonical_ref)
        .map_err(|e| anyhow::anyhow!("invalid canonical ref '{}': {e}", req.canonical_ref))?;

    let effective = state.engine.effective_item(EffectiveItemRequest {
        item_ref,
        expected_kind: req.expected_kind,
        project_root: req.project_path.map(std::path::PathBuf::from),
    })?;

    serde_json::to_value(effective).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:items/effective",
    endpoint: "items.effective",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};
