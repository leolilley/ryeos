//! `items.effective` — resolve, verify, compose, and return an effective item.
//!
//! Works for any item kind (executable or not). The engine owns the
//! resolution/trust/composition semantics; this handler is intentionally
//! only a typed service wrapper.
//!
//! ## Error codes
//!
//! The handler maps engine errors to `HandlerError` variants whose messages
//! carry a stable prefix that clients can branch on:
//!
//! | Prefix | HTTP | Meaning |
//! |--------|------|---------|
//! | `invalid canonical ref` | 400 | Malformed canonical ref string |
//! | `wrong_kind:` | 400 | `expected_kind` guard failed |
//! | `untrusted:` | 403 | Signer not in trust store |
//! | `composition_failed:` | 400 | Composer chain error |
//! | `parse_failed:` | 400 | Parser produced invalid output |
//! | (none) | 404 | Item not found in any space |
//! | (none) | 500 | Unexpected internal error |

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::EffectiveItemRequest;
use ryeos_engine::error::EngineError;
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
        .map_err(|e| HandlerError::BadRequest(format!("invalid canonical ref '{}': {e}", req.canonical_ref)))?;

    let effective = state
        .engine
        .effective_item(EffectiveItemRequest {
            item_ref,
            expected_kind: req.expected_kind,
            project_root: req.project_path.map(std::path::PathBuf::from),
        })
        .map_err(map_engine_error)?;

    serde_json::to_value(effective).map_err(Into::into)
}

/// Map typed engine errors to HTTP-appropriate handler errors with
/// stable error codes. Renderers can branch on these instead of
/// parsing error messages.
fn map_engine_error(e: EngineError) -> HandlerError {
    match &e {
        EngineError::EffectiveItemNotFound { canonical_ref: _ } => HandlerError::NotFound,
        EngineError::EffectiveItemWrongKind {
            expected,
            found,
            canonical_ref,
        } => HandlerError::BadRequest(format!(
            "wrong_kind: expected `{expected}`, got `{found}` for `{canonical_ref}`"
        )),
        EngineError::EffectiveItemUntrusted {
            canonical_ref,
            fingerprint,
        } => HandlerError::Forbidden(format!(
            "untrusted: `{canonical_ref}` (fingerprint: {fingerprint})"
        )),
        EngineError::EffectiveItemCompositionFailed { reason, .. } => {
            HandlerError::BadRequest(format!("composition_failed: {reason}"))
        }
        EngineError::EffectiveItemParseFailed { reason, .. } => {
            HandlerError::BadRequest(format!("parse_failed: {reason}"))
        }
        _ => HandlerError::Internal(e.to_string()),
    }
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
