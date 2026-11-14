//! `remote/revoke` — local CLI wrapper to revoke a grant on a remote node.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote name (default: "default").
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Local project path the CLI may forward. Remote pins are node-level
    /// records, so a project never changes which remote is used.
    #[serde(default, alias = "project")]
    pub project_path: Option<PathBuf>,
    /// Pass-through for the CLI's `--no-project` flag. Has no effect.
    #[serde(default)]
    pub no_project: bool,
    /// Fingerprint (64 lowercase hex) of the grant to revoke on the remote.
    pub fingerprint: String,
    /// Why the grant is revoked, recorded in the remote's tombstone.
    pub reason: String,
    /// Leave the revoked grants' live threads running on the remote.
    #[serde(default)]
    pub leave_running: bool,
    /// Revoke only the named grant, not the grants it granted.
    #[serde(default)]
    pub keep_delegates: bool,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let client = RemoteClient::from_named_remote(&state, &req.remote)?;
    client
        .revoke_key(
            &req.fingerprint,
            &req.reason,
            req.leave_running,
            req.keep_delegates,
        )
        .await
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/revoke",
    endpoint: "remote.revoke",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/admin"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
