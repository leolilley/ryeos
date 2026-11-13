//! `remote/push` — push project content to a remote node.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config;
use crate::remote::push::{push_project, push_project_ai_only};
use ryeos_app::ignore::IgnoreMatcher;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::project_sync::ProjectSyncScope;

/// Which principal an outbound remote call is made as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundPrincipal {
    /// The source node itself, under its own node-key grant on the remote.
    Node,
    /// The authenticated caller, under the delegation the caller signed for
    /// this remote. The remote records the work as the caller.
    Delegated,
}

impl OutboundPrincipal {
    /// The principal an outbound call is made as. An explicit choice stands;
    /// otherwise a call is made as the caller, except when the caller is the
    /// node itself, which has no delegation to present.
    pub fn resolve(
        requested: Option<Self>,
        state: &AppState,
        context: &crate::handler_context::HandlerContext,
    ) -> Self {
        requested.unwrap_or_else(|| {
            if context.fingerprint == state.identity.principal_id()
                || context.fingerprint == state.identity.fingerprint()
            {
                Self::Node
            } else {
                Self::Delegated
            }
        })
    }

    /// Wire spelling, as echoed in results.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Delegated => "delegated",
        }
    }

    /// Open the outbound client for this principal.
    pub fn client(
        self,
        state: &AppState,
        remote: &config::RemoteConfig,
        context: &crate::handler_context::HandlerContext,
    ) -> Result<RemoteClient, crate::handler_error::HandlerError> {
        match self {
            Self::Node => RemoteClient::from_remote_cfg(state, remote).map_err(Into::into),
            Self::Delegated => RemoteClient::from_remote_cfg_as_delegated(state, remote, context)
                .map_err(outbound_refusal),
        }
    }
}

/// A refused outbound client as a structured handler refusal: a typed pin or
/// delegation refusal keeps its stable code and cure.
pub fn outbound_refusal(error: anyhow::Error) -> crate::handler_error::HandlerError {
    config::typed_load_error_or(error, |error| {
        crate::handler_error::HandlerError::Forbidden(format!("{error:#}"))
    })
}

/// Open the outbound client for a named remote, as the caller under its signed
/// delegation or as the node when the caller is the node itself.
pub fn named_remote_client(
    state: &AppState,
    remote_name: &str,
    context: &crate::handler_context::HandlerContext,
) -> Result<RemoteClient> {
    let remotes = config::load_remotes(&state.config.app_root)?;
    let remote = config::get_remote(&remotes, remote_name)?;
    Ok(OutboundPrincipal::resolve(None, state, context).client(state, &remote, context)?)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote name (default: "default").
    #[serde(default = "default_remote")]
    pub remote: String,
    /// `--project <abs>` from the CLI. Required: pushing nothing
    /// is a no-op so `--no-project` is not accepted here. CLI must
    /// canonicalize before sending.
    pub project: PathBuf,
    /// Principal used to sign the remote CAS/HEAD transaction. Defaults to the
    /// authenticated caller under its signed delegation, or to the source node
    /// when the caller is the node itself.
    #[serde(default)]
    pub outbound_principal: Option<OutboundPrincipal>,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(
    req: Request,
    context: crate::handler_context::HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let path = &req.project;
    if !path.is_absolute() {
        return Err(anyhow::anyhow!(
            "project '{}' is not absolute: the CLI must canonicalize \
             before sending. The daemon's cwd is irrelevant to the caller.",
            path.display()
        ));
    }
    let canonical_abs = path.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "cannot canonicalize project '{}': {}. Ensure the path \
             exists and is accessible.",
            path.display(),
            e
        )
    })?;
    let mut project_path_for_ref = config::local_project_identity(&canonical_abs)?.to_owned();
    let abs_project_path = canonical_abs;

    // Resolve scope and remote-path-for-ref from any configured binding.
    // Default scope is `AiOnly` (see `ProjectSyncScope::default`) so an
    // unbound `remote push` ships only the `.ai/` subtree, never the
    // surrounding codebase or asset trees.
    let report = config::load_remotes_report(&state.config.app_root)?;
    let loaded_remote = config::get_loaded_remote(&report.remotes, &req.remote).ok();
    let remote_cfg = loaded_remote.as_ref().map(|loaded| loaded.config.clone());
    let principal = OutboundPrincipal::resolve(req.outbound_principal, &state, &context);
    let client = match principal {
        OutboundPrincipal::Node => {
            RemoteClient::from_named_remote(&state, &req.remote)?
        }
        OutboundPrincipal::Delegated => {
            let remote = remote_cfg.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "configured remote '{}' is required for a delegated push",
                    req.remote
                )
            })?;
            principal.client(&state, remote, &context)?
        }
    };
    let mut scope = ProjectSyncScope::default();
    if let Some(loaded) = loaded_remote.as_ref() {
        match config::resolve_loaded_project_binding(loaded, &abs_project_path) {
            Ok(binding) => {
                scope = binding.sync_scope;
                project_path_for_ref = binding.remote_project_path;
            }
            Err(_) => {
                // No binding is required for the default ai_only push; use
                // the canonical local project path as the remote path ref.
            }
        }
    }

    match scope {
        ProjectSyncScope::AiOnly => {
            // Apply remote ignore rules client-side so `.pyc`,
            // `__pycache__/`, etc. are dropped before we POST a
            // manifest the remote would reject.
            let remote_ignore = match remote_cfg.as_ref() {
                Some(cfg) => IgnoreMatcher::from_config(cfg.ingest_rules()?)?,
                None => {
                    let fetched = client.get_ingest_ignore().await.map_err(|e| {
                        anyhow::anyhow!(
                            "no remote config for '{}' and inline fetch failed: {e:#} \
                             — run `ryeos remote configure --remote {}` first",
                            req.remote,
                            req.remote
                        )
                    })?;
                    IgnoreMatcher::from_config(&fetched)?
                }
            };
            let authority = state
                .state_store
                .with_state_db(|db| db.pinned_authority())?;
            let result = push_project_ai_only(
                &client,
                &state,
                &authority,
                &abs_project_path,
                &project_path_for_ref,
                &remote_ignore,
            )
            .await?;
            Ok(serde_json::json!({
                "scope": "ai_only",
                "remote_project_path": project_path_for_ref,
                "snapshot_hash": result.snapshot_hash,
                "tree_hash": result.tree_hash,
                "tree_entries": result.tree_entries,
                "blobs_uploaded": result.blobs_uploaded,
                "blobs_skipped": result.blobs_skipped,
                // `remote push` uploads CAS objects and records the snapshot; it
                // does NOT apply the snapshot to the live project. Say so, so a
                // successful push is not mistaken for a deploy.
                "applied": false,
                "note": "objects uploaded to the remote CAS and the snapshot recorded, but NOT applied to the live project. `remote push` uploads only — it does not deploy.",
                "next_step": format!(
                    "run `ryeos remote sync-project-ai --remote {}` to apply the pushed AI content (push + apply-snapshot)",
                    req.remote
                ),
            }))
        }
        ProjectSyncScope::FullProject => {
            // Full-project push requires the remote's ingest ignore rules.
            // Fetch inline on miss, but do not persist partial/invalid remote
            // config. Operators should use `remote configure` for durable config.
            let remote_ignore = match remote_cfg {
                Some(cfg) => IgnoreMatcher::from_config(cfg.ingest_rules()?)?,
                None => {
                    let fetched = client.get_ingest_ignore().await.map_err(|e| {
                        anyhow::anyhow!(
                            "no remote config for '{}' and inline fetch failed: {e:#} \
                             — run `ryeos remote configure --remote {}` first",
                            req.remote,
                            req.remote
                        )
                    })?;
                    IgnoreMatcher::from_config(&fetched)?
                }
            };
            let authority = state
                .state_store
                .with_state_db(|db| db.pinned_authority())?;
            let result = push_project(
                &client,
                &state,
                &authority,
                &abs_project_path,
                &project_path_for_ref,
                &remote_ignore,
            )
            .await?;
            Ok(serde_json::json!({
                "scope": "full_project",
                "remote_project_path": project_path_for_ref,
                "snapshot_hash": result.snapshot_hash,
                "tree_hash": result.tree_hash,
                "tree_entries": result.tree_entries,
                "blobs_uploaded": result.blobs_uploaded,
                "blobs_skipped": result.blobs_skipped,
                // See ai_only branch: push uploads only, it does not deploy.
                "applied": false,
                "note": "objects uploaded to the remote CAS and the snapshot recorded, but NOT applied to the live project worktree. `remote push` uploads only — it does not deploy.",
            }))
        }
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/push",
    endpoint: "remote.push",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/push"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::{OutboundPrincipal, Request};

    #[test]
    fn outbound_principal_is_explicit_and_closed() {
        assert!(matches!(
            serde_json::from_str::<OutboundPrincipal>(r#""delegated""#).unwrap(),
            OutboundPrincipal::Delegated
        ));
        assert!(matches!(
            serde_json::from_str::<OutboundPrincipal>(r#""node""#).unwrap(),
            OutboundPrincipal::Node
        ));
        assert!(serde_json::from_str::<OutboundPrincipal>(r#""caller""#).is_err());
        assert!(serde_json::from_str::<OutboundPrincipal>(r#""configured_operator""#).is_err());
    }

    #[test]
    fn signed_service_payload_exposes_outbound_principal() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "remote": "hosted",
            "project": "/project",
            "outbound_principal": "delegated"
        }))
        .unwrap();
        assert_eq!(request.outbound_principal, Some(OutboundPrincipal::Delegated));
        let defaulted: Request = serde_json::from_value(serde_json::json!({
            "remote": "hosted",
            "project": "/project"
        }))
        .unwrap();
        assert_eq!(defaulted.outbound_principal, None);
    }
}
