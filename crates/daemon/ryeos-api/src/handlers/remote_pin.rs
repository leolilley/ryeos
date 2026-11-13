//! `remote/pin` — pin a remote node from its descriptor, without contacting
//! it.
//!
//! `remote configure` dials the remote to read its key, site and ingest
//! rules. A node that cannot reach the remote (a hosted controller behind an
//! origin's NAT) pins it from the descriptor the remote's operator exported
//! instead: the descriptor is the trust anchor, and it must carry the node's
//! site and vault fingerprint. A descriptor without ingest rules pins a
//! remote this node never pushes to.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use crate::remote::config;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Name to pin the remote under; defaults to the descriptor's name.
    #[serde(default)]
    pub remote: Option<String>,
    /// Path to the remote's descriptor YAML.
    pub descriptor: PathBuf,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> HandlerResult<Value> {
    let descriptor = config::RemoteDescriptor::from_path(&req.descriptor)
        .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?;
    let name = req
        .remote
        .clone()
        .or_else(|| descriptor.name.clone())
        .ok_or_else(|| {
            HandlerError::BadRequest(
                "remote name required: pass a remote name or descriptor.name".to_owned(),
            )
        })?;
    config::pin_remote(state.config.runtime_root().as_path(), &name, &descriptor)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/pin",
    endpoint: "remote.pin",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.remote/pin"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};
