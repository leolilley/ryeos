//! `trust.list` and `trust.show` — read-only views of the node's signer
//! registry: records, statuses, scopes and the key-event chain.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ListRequest {}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShowRequest {
    pub fingerprint: String,
}

fn head(chain: &[ryeos_engine::trust::TrustEvent]) -> Value {
    match chain.last() {
        Some(event) => json!({ "seq": event.seq, "digest": event.digest() }),
        None => Value::Null,
    }
}

/// Load the registry through the same verifying reader the daemon boots
/// with, so a view never shows a record the node would refuse.
fn view(state: &AppState) -> Result<ryeos_node::trust_registry::RegistryView> {
    ryeos_node::trust_registry::load_view(state.config.runtime_root().as_path())
        .context("read the node signer registry")
}

pub async fn handle_list(_req: ListRequest, state: Arc<AppState>) -> Result<Value> {
    let view = view(&state)?;
    let node_fingerprint =
        ryeos_engine::trust::TrustStore::load_node_registry(state.config.runtime_root().as_path())?
            .node_fingerprint();
    Ok(json!({
        "node_fingerprint": node_fingerprint,
        "signers": view.records,
        "registry_digest": view.registry_digest,
        "event_head": head(&view.chain),
    }))
}

pub async fn handle_show(req: ShowRequest, state: Arc<AppState>) -> Result<Value> {
    let view = view(&state)?;
    let Some(record) = view
        .records
        .iter()
        .find(|record| record.fingerprint == req.fingerprint)
    else {
        bail!(
            "trust_signer_unknown: no signer {} is pinned. List signers with `ryeos trust list`",
            req.fingerprint
        );
    };
    let events: Vec<Value> = view
        .chain
        .iter()
        .filter(|event| event.signer == req.fingerprint)
        .map(|event| {
            json!({
                "seq": event.seq,
                "digest": event.digest(),
                "kind": event.kind.as_str(),
                "request_digest": event.request_digest,
                "operator": event.operator,
            })
        })
        .collect();
    Ok(json!({
        "signer": record,
        "events": events,
        "event_head": head(&view.chain),
    }))
}

pub const LIST_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:trust/list",
    endpoint: "trust.list",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.trust/list"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: ListRequest = if params.is_null() {
                ListRequest::default()
            } else {
                serde_json::from_value(params)?
            };
            handle_list(req, state).await
        })
    },
};

pub const SHOW_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:trust/show",
    endpoint: "trust.show",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.trust/show"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: ShowRequest = serde_json::from_value(params)?;
            handle_show(req, state).await
        })
    },
};
