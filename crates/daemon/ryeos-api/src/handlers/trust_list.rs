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

/// One signer record as the views show it: the record plus the time its key
/// first entered the registry, read from the signing time of its first
/// pinning event.
fn signer_json(
    view: &ryeos_node::trust_registry::RegistryView,
    fingerprint: &str,
    record: &impl serde::Serialize,
) -> Result<Value> {
    let mut value = serde_json::to_value(record)?;
    let graph = ryeos_engine::trust::SuccessionGraph::build(&view.records, &view.chain);
    if let Value::Object(map) = &mut value {
        map.insert("pinned_at".to_string(), json!(view.pinned_at(fingerprint)));
        // The project owner this key's signatures speak for, and whether it
        // may still act for it (retired and revoked keys do not).
        map.insert(
            "owner".to_string(),
            json!(graph.signing_principal_of(fingerprint).ok().flatten()),
        );
        map.insert(
            "acts_for_owner".to_string(),
            json!(graph.ownership_of(fingerprint).is_ok()),
        );
    }
    Ok(value)
}

fn signers_json(view: &ryeos_node::trust_registry::RegistryView) -> Result<Vec<Value>> {
    view.records
        .iter()
        .map(|record| signer_json(view, &record.fingerprint, record))
        .collect()
}

fn events_json(view: &ryeos_node::trust_registry::RegistryView, fingerprint: &str) -> Vec<Value> {
    view.chain
        .iter()
        .zip(view.signed_at.iter())
        .filter(|(event, _)| event.signer == fingerprint)
        .map(|(event, signed_at)| {
            json!({
                "seq": event.seq,
                "digest": event.digest(),
                "kind": event.kind.as_str(),
                "request_digest": event.request_digest,
                "operator": event.operator,
                "signed_at": signed_at,
            })
        })
        .collect()
}

pub async fn handle_list(_req: ListRequest, state: Arc<AppState>) -> Result<Value> {
    let view = view(&state)?;
    let node_fingerprint =
        ryeos_engine::trust::TrustStore::load_node_registry(state.config.runtime_root().as_path())?
            .node_fingerprint();
    Ok(json!({
        "node_fingerprint": node_fingerprint,
        "signers": signers_json(&view)?,
        "owners": ryeos_engine::trust::SuccessionGraph::build(&view.records, &view.chain)
            .principals(),
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
    let events = events_json(&view, &req.fingerprint);
    Ok(json!({
        "signer": signer_json(&view, &record.fingerprint, record)?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::trust::{TRUST_EVENT_SCHEMA, TrustEvent, TrustEventKind};
    use ryeos_node::trust_registry::RegistryView;

    fn event(seq: u64, kind: TrustEventKind, signer: &str) -> TrustEvent {
        TrustEvent {
            schema: TRUST_EVENT_SCHEMA.to_string(),
            seq,
            previous: None,
            kind,
            signer: signer.to_string(),
            record_digest: "b".repeat(64),
            request_digest: "c".repeat(64),
            operator: "d".repeat(64),
            predecessor: None,
        }
    }

    fn view() -> RegistryView {
        RegistryView {
            records: Vec::new(),
            chain: vec![
                event(1, TrustEventKind::Pinned, "k1"),
                event(2, TrustEventKind::Retired, "k1"),
                event(3, TrustEventKind::Pinned, "k1"),
                event(4, TrustEventKind::Pinned, "k2"),
            ],
            signed_at: vec![
                "2026-01-01T00:00:00Z".into(),
                "2026-02-02T00:00:00Z".into(),
                "2026-03-03T00:00:00Z".into(),
                "2026-04-04T00:00:00Z".into(),
            ],
            registry_digest: String::new(),
        }
    }

    #[test]
    fn events_carry_their_signing_time() {
        let events = events_json(&view(), "k1");
        let times: Vec<&str> = events
            .iter()
            .map(|e| e["signed_at"].as_str().unwrap())
            .collect();
        assert_eq!(
            times,
            [
                "2026-01-01T00:00:00Z",
                "2026-02-02T00:00:00Z",
                "2026-03-03T00:00:00Z"
            ]
        );
    }

    #[test]
    fn pinned_at_is_the_first_pin_even_after_retire_and_repin() {
        let v = view();
        let signer = signer_json(&v, "k1", &json!({"fingerprint": "k1"})).unwrap();
        assert_eq!(signer["pinned_at"], "2026-01-01T00:00:00Z");
        let signer = signer_json(&v, "k2", &json!({"fingerprint": "k2"})).unwrap();
        assert_eq!(signer["pinned_at"], "2026-04-04T00:00:00Z");
    }
}
