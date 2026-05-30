//! `federation/heads/list` — list authorized namespace-neutral signed heads.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

fn default_limit() -> usize {
    100
}

#[derive(Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    pub prefix: String,
    pub limit: usize,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            limit: default_limit(),
        }
    }
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    if req.prefix.is_empty() {
        anyhow::bail!("federation heads list requires a non-empty prefix");
    }
    let limit = req.limit.min(500);
    let heads = state
        .state_store
        .with_state_db(|db| db.list_generic_head_refs(&req.prefix))?;
    let truncated = heads.len() > limit;
    Ok(serde_json::json!({
        "prefix": req.prefix,
        "limit": limit,
        "truncated": truncated,
        "heads": heads
            .into_iter()
            .take(limit)
            .map(generic_head_to_json)
            .collect::<Vec<_>>(),
    }))
}

fn generic_head_to_json(head: ryeos_state::GenericHeadRef) -> Value {
    serde_json::json!({
        "namespace": head.namespace,
        "name": head.name,
        "ref_path": head.ref_path,
        "target_hash": head.target_hash,
        "signer": head.signer,
        "updated_at": head.updated_at,
        "signed_ref": head.signed_ref,
    })
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:federation/heads/list",
    endpoint: "federation.heads.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.federation.heads.list"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            handle(req, state).await
        })
    },
};
