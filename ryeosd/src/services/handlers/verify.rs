//! `verify` — resolve and verify an item through the daemon's engine.
//!
//! Returns the trust class (TRUSTED / UNTRUSTED / UNSIGNED) plus
//! the resolved metadata. Wraps `Engine::resolve` + `Engine::verify`
//! using `state.engine`.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::Value;

use ryeos_engine::{
    canonical_ref::CanonicalRef,
    contracts::{
        EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext, TrustClass,
    },
};

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(Debug, serde::Deserialize)]
pub struct Request {
    /// Canonical ref to verify (e.g. `service:system/status`).
    pub item_ref: String,
}

#[derive(Debug, serde::Serialize)]
pub struct VerifyReport {
    pub item_ref: String,
    pub kind: String,
    pub resolved_path: String,
    pub space: String,
    pub content_hash: String,
    pub trust_class: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let canonical_ref = CanonicalRef::parse(&req.item_ref)
        .map_err(|e| anyhow!("failed to parse item ref `{}`: {e}", req.item_ref))?;

    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: state.identity.principal_id(),
            scopes: vec!["bundle.read".to_string()],
        }),
        project_context: ProjectContext::None,
        current_site_id: "site:local".into(),
        origin_site_id: "site:local".into(),
        execution_hints: ExecutionHints::default(),
        validate_only: false,
    };

    let resolved = match state.engine.resolve(&plan_ctx, &canonical_ref) {
        Ok(item) => item,
        Err(e) => {
            return serde_json::to_value(VerifyReport {
                item_ref: req.item_ref,
                kind: canonical_ref.kind.clone(),
                resolved_path: String::new(),
                space: String::new(),
                content_hash: String::new(),
                trust_class: String::new(),
                status: "FAILED".into(),
                error: Some(format!("{e}")),
            })
            .map_err(Into::into);
        }
    };

    let resolved_path = resolved.source_path.display().to_string();
    let space = resolved.source_space.as_str().to_string();
    let content_hash = resolved.content_hash.clone();
    let kind = resolved.kind.clone();

    let report = match state.engine.verify(&plan_ctx, resolved) {
        Ok(verified) => {
            let trust_class = match verified.trust_class {
                TrustClass::Trusted => "TRUSTED",
                TrustClass::Untrusted => "UNTRUSTED",
                TrustClass::Unsigned => "UNSIGNED",
            };
            VerifyReport {
                item_ref: req.item_ref,
                kind,
                resolved_path,
                space,
                content_hash,
                trust_class: trust_class.to_string(),
                status: "SUCCESS".into(),
                error: None,
            }
        }
        Err(e) => VerifyReport {
            item_ref: req.item_ref,
            kind,
            resolved_path,
            space,
            content_hash,
            trust_class: String::new(),
            status: "VERIFICATION_FAILED".into(),
            error: Some(format!("{e}")),
        },
    };

    serde_json::to_value(report).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:verify",
    endpoint: "verify",
    availability: ServiceAvailability::Both,
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)
                .map_err(|e| anyhow::anyhow!("invalid verify params: {e}"))?;
            handle(req, state).await
        })
    },
};
