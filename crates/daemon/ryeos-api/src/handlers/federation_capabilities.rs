//! `federation/capabilities` — advertise distributed-substrate protocol support.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

pub async fn handle(_req: Request, state: Arc<AppState>) -> Result<Value> {
    Ok(serde_json::json!({
        "protocol": {
            "name": "ryeos-distributed-substrate",
            "versions": [1],
            "preferred_version": 1,
        },
        "identity": {
            "principal_id": state.identity.principal_id(),
            "fingerprint": state.identity.fingerprint().to_string(),
            "site_id": state.threads.site_id().to_string(),
        },
        "object_kinds": [
            "project_snapshot",
            "source_manifest",
            "item_source",
            "chain_state",
            "thread_snapshot",
            "thread_event",
            "attestation",
        ],
        "services": {
            "objects": {
                "closure_describe": true,
                "closure_get": true,
                "closure_put": false,
                "closure_verify": false,
            },
            "admission": {
                "submit": true,
                "status": true,
                "attestations_for_subject": false,
                "policies": ["local-node-v1"],
            },
            "sync_jobs": {
                "list": true,
                "inspect": true,
                "attempts": true,
                "async_submit": false,
                "resume": false,
            },
            "heads": {
                "generic_refs": true,
                "federated_list": false,
                "anti_replay_sequence": false,
            },
            "federation": {
                "capabilities": true,
                "head_exchange": false,
                "subscriptions": false,
            },
        },
        "limits": {
            "max_roots_per_closure_request": 64,
            "default_max_objects_per_closure": 4096,
            "default_max_blobs_per_closure": 4096,
            "default_max_total_blob_bytes": 256 * 1024 * 1024,
        },
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:federation/capabilities",
    endpoint: "federation.capabilities",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
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
