//! `federation/heads/list` — list authorized namespace-neutral signed heads.

use std::sync::Arc;

use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

fn default_limit() -> usize {
    100
}

#[derive(Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    pub chain_root_id: Option<String>,
    pub prefix: String,
    pub limit: usize,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            chain_root_id: None,
            prefix: String::new(),
            limit: default_limit(),
        }
    }
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let limit = req.limit.min(500);
    let (response_prefix, heads) = match (req.chain_root_id.as_deref(), req.prefix.as_str()) {
        (None, prefix) if is_federation_safe_prefix(prefix) => (
            prefix.to_owned(),
            state
                .state_store
                .with_state_db(|db| db.list_generic_head_refs(prefix))
                .map_err(internal)?,
        ),
        (Some(chain_root_id), "") => {
            validate_chain_root_component(chain_root_id)?;
            ryeos_app::operator_external_content::require_configured_operator(&state, &ctx)
                .map_err(|_| HandlerError::Forbidden("configured operator required".into()))?;
            let placement_thread_id = state
                .state_store
                .current_chain_placement_thread_id(chain_root_id)
                .map_err(internal)?
                .ok_or(HandlerError::NotFound)?;
            let thread = state
                .state_store
                .get_thread(&placement_thread_id)
                .map_err(internal)?
                .ok_or(HandlerError::NotFound)?;
            if thread.chain_root_id != chain_root_id {
                return Err(internal("authoritative chain placement escaped its root"));
            }
            ctx.require_owner(thread.requested_by.as_deref())?;
            let head = state
                .state_store
                .with_state_db(|db| db.read_generic_head_ref("chains", chain_root_id))
                .map_err(internal)?
                .ok_or(HandlerError::NotFound)?;
            let name = format!("{chain_root_id}/head");
            (
                format!("chains/{chain_root_id}"),
                vec![ryeos_state::GenericHeadRef {
                    namespace: "chains".to_owned(),
                    name,
                    ref_path: head.ref_path.clone(),
                    target_hash: head.target_hash.clone(),
                    signer: head.signer.clone(),
                    updated_at: head.updated_at.clone(),
                    signed_ref: head,
                }],
            )
        }
        _ => {
            return Err(HandlerError::BadRequest(
                "federation heads requires either an admissions prefix or one exact chain_root_id"
                    .into(),
            ));
        }
    };
    let expected_signer = state.identity.fingerprint().to_string();
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
            Ok(head)
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map_err(internal)?;
    let truncated = verified_heads.len() > limit;
    Ok(serde_json::json!({
        "prefix": response_prefix,
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

fn validate_chain_root_component(chain_root_id: &str) -> Result<(), HandlerError> {
    if chain_root_id.is_empty()
        || matches!(chain_root_id, "." | "..")
        || chain_root_id.len() > 128
        || !chain_root_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
    {
        return Err(HandlerError::BadRequest(
            "chain_root_id is not a canonical ref component".into(),
        ));
    }
    Ok(())
}

fn internal(error: impl std::fmt::Display) -> HandlerError {
    HandlerError::Internal(error.to_string())
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
    required_caps: &["ryeos.execute.service.federation/heads/list"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            handle(req, ctx, state).await.map_err(anyhow::Error::new)
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

    #[test]
    fn exact_chain_root_is_one_safe_ref_component() {
        assert!(super::validate_chain_root_component("T-root:one").is_ok());
        assert!(super::validate_chain_root_component("T/root").is_err());
        assert!(super::validate_chain_root_component("..").is_err());
    }
}
