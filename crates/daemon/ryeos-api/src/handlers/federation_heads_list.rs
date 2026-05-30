//! `federation/heads/list` — list authorized namespace-neutral signed heads.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::TrustStore;

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
    if !is_federation_safe_prefix(&req.prefix) {
        anyhow::bail!(
            "federation heads list prefix is not exportable: {}",
            req.prefix
        );
    }
    let limit = req.limit.min(500);
    let heads = state
        .state_store
        .with_state_db(|db| db.list_generic_head_refs(&req.prefix))?;
    let expected_signer = state.identity.fingerprint().to_string();
    let mut trust_store = TrustStore::new();
    trust_store.insert(
        expected_signer.clone(),
        state.identity.verifying_key().clone(),
    );
    let verified_heads = heads
        .into_iter()
        .map(|head| {
            if head.signer != expected_signer {
                anyhow::bail!(
                    "ref {} is signed by {}, not local node {}",
                    head.ref_path,
                    head.signer,
                    expected_signer
                );
            }
            ryeos_state::verify_signed_ref(&head.signed_ref, &trust_store)?;
            Ok(head)
        })
        .collect::<Result<Vec<_>>>()?;
    let truncated = verified_heads.len() > limit;
    Ok(serde_json::json!({
        "prefix": req.prefix,
        "limit": limit,
        "truncated": truncated,
        "heads": verified_heads
            .into_iter()
            .take(limit)
            .map(generic_head_to_json)
            .collect::<Vec<_>>(),
    }))
}

fn is_federation_safe_prefix(prefix: &str) -> bool {
    prefix == "admissions" || prefix.starts_with("admissions/")
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

#[cfg(test)]
mod tests {
    #[test]
    fn federation_head_prefixes_are_allowlisted() {
        assert!(super::is_federation_safe_prefix("admissions"));
        assert!(super::is_federation_safe_prefix("admissions/local-node-v1"));
        assert!(!super::is_federation_safe_prefix(""));
        assert!(!super::is_federation_safe_prefix("chains"));
        assert!(!super::is_federation_safe_prefix("projects/fp/head"));
    }
}
