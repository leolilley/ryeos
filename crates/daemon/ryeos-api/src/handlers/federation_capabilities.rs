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
    let transfer = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>(
    )?;
    Ok(serde_json::json!({
        "protocol": {
            "name": "ryeos-distributed-substrate",
            "versions": [crate::remote::client::DISTRIBUTED_SUBSTRATE_PROTOCOL_VERSION],
            "preferred_version": crate::remote::client::DISTRIBUTED_SUBSTRATE_PROTOCOL_VERSION,
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
                "attestations_for_subject": true,
                "policies": [super::admission_submit::LOCAL_ADMISSION_POLICY],
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
                "federated_list": true,
                "exportable_prefixes": ["admissions"],
                "anti_replay_sequence": false,
            },
            "federation": {
                "capabilities": true,
                "head_exchange": true,
                "subscriptions": false,
            },
        },
        "limits": {
            "max_roots_per_closure_request": transfer.max_roots,
            "max_objects_per_closure": transfer.max_objects,
            "max_blobs_per_closure": transfer.max_blobs,
            "max_object_bytes": transfer.max_object_bytes,
            "max_total_object_bytes": transfer.max_total_object_bytes,
            "max_blob_bytes": transfer.max_blob_bytes,
            "max_total_blob_bytes": transfer.max_total_blob_bytes,
            "max_response_bytes": transfer.max_response_bytes,
            "max_links_per_object": transfer.max_links_per_object,
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
