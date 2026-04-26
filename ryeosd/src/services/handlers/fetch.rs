//! `fetch` — resolve, optionally verify, and read an item through the
//! daemon's engine. Mirrors `ryeos_tools::actions::fetch::run_fetch` but
//! uses the already-loaded `state.engine`.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
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
    pub item_ref: String,
    #[serde(default)]
    pub with_content: bool,
    #[serde(default)]
    pub verify: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct FetchReport {
    pub item_ref: String,
    pub kind: String,
    pub resolved_path: String,
    pub resolved_from: String,
    pub space: String,
    pub content_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_status: Option<String>,
    pub shadowed_count: usize,
    pub fetch_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
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
            return serde_json::to_value(FetchReport {
                item_ref: req.item_ref,
                kind: canonical_ref.kind.clone(),
                resolved_path: String::new(),
                resolved_from: String::new(),
                space: String::new(),
                content_hash: String::new(),
                signature_status: None,
                shadowed_count: 0,
                fetch_status: "FAILED".into(),
                content: None,
                error: Some(format!("{e}")),
            })
            .map_err(Into::into);
        }
    };

    let signature_status = if req.verify {
        match state.engine.verify(&plan_ctx, resolved.clone()) {
            Ok(verified) => Some(
                match verified.trust_class {
                    TrustClass::Trusted => "TRUSTED",
                    TrustClass::Untrusted => "UNTRUSTED",
                    TrustClass::Unsigned => "UNSIGNED",
                }
                .to_string(),
            ),
            Err(e) => Some(format!("VERIFICATION_FAILED: {e}")),
        }
    } else {
        None
    };

    let content = std::fs::read_to_string(&resolved.source_path)
        .with_context(|| format!("failed to read item content from {:?}", resolved.source_path))?;

    let report = FetchReport {
        item_ref: req.item_ref,
        kind: resolved.kind.clone(),
        resolved_path: resolved.source_path.display().to_string(),
        resolved_from: resolved.resolved_from.clone(),
        space: resolved.source_space.as_str().to_string(),
        content_hash: resolved.content_hash.clone(),
        signature_status,
        shadowed_count: resolved.shadowed.len(),
        fetch_status: "SUCCESS".into(),
        content: if req.with_content { Some(content) } else { None },
        error: None,
    };

    serde_json::to_value(report).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:fetch",
    endpoint: "fetch",
    availability: ServiceAvailability::Both,
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)
                .map_err(|e| anyhow::anyhow!("invalid fetch params: {e}"))?;
            handle(req, state).await
        })
    },
};
