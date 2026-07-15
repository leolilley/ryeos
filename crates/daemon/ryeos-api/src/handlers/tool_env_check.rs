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
//! Scope: item `required_secrets` plus symbolic requirements returned by the
//! target's signed generic launch contract. The same threadless preparation
//! pass used by accepted launch validates bindings, configuration, and handler
//! output without exposing secret values or runtime-specific concepts here.

use std::collections::BTreeMap;
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
    pub ref_bindings: BTreeMap<String, String>,
    /// Project root the item resolves against (also the `.env` overlay root).
    /// Bound by the CLI from the discovered project; absent when run outside
    /// a project.
    #[serde(default)]
    pub project_path: Option<String>,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    ryeos_executor::execution::launch_preparation::validate_ref_bindings(&req.ref_bindings)
        .map_err(map_dispatch_error)?;

    let requested_project_path = req.project_path.ok_or_else(|| {
        HandlerError::BadRequest(
            "env-check requires a project: run inside a project directory".into(),
        )
    })?;
    let project_path = std::fs::canonicalize(&requested_project_path).map_err(|error| {
        HandlerError::BadRequest(format!(
            "could not resolve project path `{requested_project_path}`: {error}"
        ))
    })?;
    if !project_path.is_dir() {
        return Err(HandlerError::BadRequest(format!(
            "project path is not a directory: {}",
            project_path.display()
        )));
    }

    let canonical =
        ryeos_engine::canonical_ref::CanonicalRef::parse(&req.item_ref).map_err(|e| {
            HandlerError::BadRequest(format!("invalid item_ref `{}`: {e}", req.item_ref))
        })?;

    // The report leaks secret presence/source, so the caller must hold the same
    // execute capability for the TARGET item that a real launch requires — being
    // allowed to call env-check is not enough on its own.
    let required_cap =
        ryeos_runtime::authorizer::canonical_cap(&canonical.kind, &canonical.bare_id, "execute");
    let policy =
        ryeos_runtime::authorizer::AuthorizationPolicy::require_all(&[required_cap.as_str()]);
    state
        .authorizer
        .authorize(&ctx.scopes, &policy)
        .map_err(|_| {
            HandlerError::Forbidden(format!("missing required capability: {required_cap}"))
        })?;
    for (name, item_ref) in &req.ref_bindings {
        let bound = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref).map_err(|error| {
            HandlerError::BadRequest(format!("invalid ref_bindings.{name}: {error}"))
        })?;
        let required = ryeos_runtime::authorizer::canonical_cap(
            &bound.kind,
            &bound.bare_id,
            "execute",
        );
        let policy = ryeos_runtime::authorizer::AuthorizationPolicy::require(&required);
        state
            .authorizer
            .authorize(&ctx.scopes, &policy)
            .map_err(|_| {
                HandlerError::Forbidden(format!(
                    "missing required capability for ref binding '{name}': {required}"
                ))
            })?;
    }

    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: ctx.fingerprint.clone(),
            scopes: ctx.scopes.clone(),
        }),
        project_context: ProjectContext::LocalPath {
            path: project_path.clone(),
        },
        current_site_id: state.threads.site_id().to_string(),
        origin_site_id: state.threads.site_id().to_string(),
        execution_hints: Default::default(),
        validate_only: true,
    };

    // Resolve AND verify the trust chain, exactly like a real launch, before
    // reading declared metadata.
    let verified = ryeos_executor::executor::resolve_and_verify(
        &state.engine,
        &plan_ctx,
        &req.item_ref,
        Some("env-check target"),
    )
    .map_err(|e| HandlerError::BadRequest(format!("could not verify `{}`: {e:#}", req.item_ref)))?;

    let mut names = verified.resolved.metadata.required_secrets.clone();
    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: ctx.fingerprint.clone(),
        caller_scopes: ctx.scopes.clone(),
        engine: state.engine.clone(),
        plan_ctx: plan_ctx.clone(),
        requested_call: None,
    };
    let applicability = ryeos_executor::dispatch::launch_contract_applicability(
        &req.item_ref,
        &exec_ctx,
    )
    .map_err(map_dispatch_error)?;
    let prepared = ryeos_executor::dispatch::prepare_launch_contract(
        &applicability,
        &verified.resolved,
        &req.ref_bindings,
        std::path::Path::new(&project_path),
        &exec_ctx,
    )
    .map_err(map_dispatch_error)?;
    let launch_contract_applied = prepared.is_some();
    if let Some(prepared) = prepared {
        names.extend(
            prepared
                .required_secrets
                .into_iter()
                .map(|requirement| requirement.name),
        );
    }
    names.sort();
    names.dedup();
    let dotenv_dirs = ryeos_app::vault::dotenv_search_dirs(Some(project_path.as_path()));
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

    // Import dry-run via the shared probe (also used by `ryeos doctor`): for a
    // python tool, reproduce the launch interpreter + sys.path and attempt the
    // import (without calling `execute`), so an empty `.venv` or a
    // `ModuleNotFoundError` surfaces here rather than at first run. The probe
    // runs a bounded subprocess, so it goes off the async runtime.
    let import_report = {
        let engine = state.engine.clone();
        let sandbox = state.sandbox.clone();
        let sandbox_bundle_roots = engine
            .resolution_roots(Some(project_path.clone()))
            .ordered
            .iter()
            .filter(|root| root.space == ryeos_engine::contracts::ItemSpace::Bundle)
            .filter_map(|root| root.ai_root.parent().map(std::path::Path::to_path_buf))
            .collect::<Vec<_>>();
        let sandbox_node_trusted_keys_dir = state.config.runtime_root().trusted_keys_dir();
        let sandbox_verified_code = [ryeos_engine::sandbox::SandboxVerifiedCode {
            source_path: verified.resolved.source_path.clone(),
            content_hash: verified.resolved.content_hash.clone(),
        }];
        let sandbox_item_ref = req.item_ref.clone();
        let probe_names = names.clone();
        tokio::task::spawn_blocking(move || {
            ryeos_app::env_probe::import_dry_run(
                &engine,
                &plan_ctx,
                &verified,
                &probe_names,
                &sandbox,
                ryeos_engine::sandbox::SandboxLaunchContext {
                    project_path: &project_path,
                    project_authority: ryeos_engine::sandbox::SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &sandbox_bundle_roots,
                    node_trusted_keys_dir: Some(&sandbox_node_trusted_keys_dir),
                    verified_code: &sandbox_verified_code,
                    item_ref: &sandbox_item_ref,
                    thread_id: "tool-env-check",
                },
            )
        })
        .await
        .unwrap_or_else(|e| {
            serde_json::json!({
                "import_check": "unavailable",
                "import_check_reason": format!("import probe task failed: {e}"),
            })
        })
    };

    let mut response = serde_json::json!({
        "item_ref": req.item_ref,
        "kind": canonical.kind,
        "secrets": secrets,
        "missing": missing,
        "launch_contract_applied": launch_contract_applied,
    });
    if let (Some(obj), Some(extra)) = (response.as_object_mut(), import_report.as_object()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    Ok(response)
}

fn map_dispatch_error(error: ryeos_executor::dispatch_error::DispatchError) -> HandlerError {
    use axum::http::StatusCode;

    let message = format!("{}: {error}", error.code());
    match error.http_status() {
        StatusCode::FORBIDDEN => HandlerError::Forbidden(message),
        StatusCode::NOT_FOUND => HandlerError::NotFound,
        StatusCode::CONFLICT => HandlerError::Conflict(message),
        status if status.is_server_error() => HandlerError::Internal(message),
        _ => HandlerError::BadRequest(message),
    }
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
