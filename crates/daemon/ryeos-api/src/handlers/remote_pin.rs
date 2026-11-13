//! `remote/pin` — pin a remote node from its signed descriptor, without
//! contacting it.
//!
//! `remote configure` dials the remote to read its key, site and ingest
//! rules. A node that cannot reach the remote (a hosted controller pinning its
//! origin behind NAT) pins it from the descriptor the remote's operator
//! exported instead. The descriptor is a version 2 document signed by the node
//! key it pins; it carries the node's site and vault fingerprint, and no URL
//! when the remote is inbound-only. The pin is written as a node-signed record.
//! A descriptor without ingest rules pins a remote this node never pushes to.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::handler_context::HandlerContext;
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
    /// Path to the remote's signed descriptor.
    pub descriptor: PathBuf,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    // A pin decides whose signatures the node accepts from that remote, so
    // only the configured local operator may make one.
    ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)
        .context("remote pin requires the configured local operator")?;
    let text = std::fs::read_to_string(&req.descriptor).with_context(|| {
        format!(
            "failed to read remote descriptor {}",
            req.descriptor.display()
        )
    })?;
    let descriptor = config::RemoteDescriptor::parse_signed(&text)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("invalid remote descriptor: {}", req.descriptor.display()))?;
    let name = req.remote.unwrap_or_else(|| descriptor.name.clone());
    let digest = lillux::cas::canonical_json(&serde_json::json!({
        "remote": name,
        "descriptor": lillux::sha256_hex(text.as_bytes()),
    }))
    .ok()
    .map(|canonical| lillux::cas::sha256_hex(canonical.as_bytes()));
    config::pin_remote(
        state.config.runtime_root().as_path(),
        state.identity.signing_key(),
        &name,
        &text,
        digest,
    )
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/pin",
    endpoint: "remote.pin",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/pin"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
