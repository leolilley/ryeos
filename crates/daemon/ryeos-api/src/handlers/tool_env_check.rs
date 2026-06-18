//! `tool/env-check` — report which source would satisfy each declared secret
//! of an item, WITHOUT running it. Names and sources only, never values.
//!
//! Resolves the item against the live engine + the caller's project, reads its
//! declared `required_secrets`, and reports per-secret provenance
//! (vault / host env / which `.env` / missing) via
//! `vault::resolve_secret_sources` — which mirrors the real launch precedence.
//!
//! DaemonOnly: the authoritative host-env source is the daemon's process
//! environment, so the report must come from the daemon, not an offline CLI.
//!
//! Scope (v1): item `required_secrets`. A directive's provider `auth.env_var`
//! is resolved separately at launch (`preflight_inject_provider_secret`) and is
//! not yet enumerated here — a follow-up will add it via the same resolver.

use std::sync::Arc;

use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_app::vault::SecretSource;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The item whose declared secrets to check (e.g. `tool:foo/bar`).
    pub item_ref: String,
    /// Project root the item resolves against (also the `.env` overlay root).
    /// Bound by the CLI from the discovered project; absent when run outside
    /// a project.
    #[serde(default)]
    pub project_path: Option<String>,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> HandlerResult<Value> {
    ctx.require_verified()?;

    let project_path = req.project_path.ok_or_else(|| {
        HandlerError::BadRequest(
            "env-check requires a project: run inside a project directory".into(),
        )
    })?;

    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: ctx.fingerprint.clone(),
            scopes: ctx.scopes.clone(),
        }),
        project_context: ProjectContext::LocalPath {
            path: std::path::PathBuf::from(&project_path),
        },
        current_site_id: state.threads.site_id().to_string(),
        origin_site_id: state.threads.site_id().to_string(),
        execution_hints: Default::default(),
        validate_only: true,
    };

    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(&req.item_ref)
        .map_err(|e| HandlerError::BadRequest(format!("invalid item_ref `{}`: {e}", req.item_ref)))?;
    let resolved = state.engine.resolve(&plan_ctx, &canonical).map_err(|e| {
        HandlerError::BadRequest(format!("could not resolve `{}`: {e}", req.item_ref))
    })?;

    let names = resolved.metadata.required_secrets.clone();
    let dotenv_dirs =
        ryeos_app::vault::dotenv_search_dirs(Some(std::path::Path::new(&project_path)));
    let report = ryeos_app::vault::resolve_secret_sources(
        state.vault.as_ref(),
        &ctx.fingerprint,
        &names,
        &dotenv_dirs,
    )
    .map_err(|e| HandlerError::Internal(e.to_string()))?;

    let secrets: Vec<Value> = report
        .iter()
        .map(|(name, source)| {
            let mut obj = serde_json::json!({ "name": name, "source": source.label() });
            if let SecretSource::Dotenv(dir) = source {
                obj["dotenv_dir"] = Value::String(dir.display().to_string());
            }
            obj
        })
        .collect();
    let missing: Vec<&str> = report
        .iter()
        .filter(|(_, s)| matches!(s, SecretSource::Missing))
        .map(|(n, _)| n.as_str())
        .collect();

    Ok(serde_json::json!({
        "item_ref": req.item_ref,
        "secrets": secrets,
        "missing": missing,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:tool/env-check",
    endpoint: "tool.env-check",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.tool/env-check"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
