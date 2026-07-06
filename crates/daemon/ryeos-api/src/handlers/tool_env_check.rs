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
//! Scope: item `required_secrets` PLUS runtime-derived provider auth. When
//! the target's runtime declares the `provider_snapshot` envelope field
//! (directives), the provider is resolved through the same preflight the
//! launch path uses and its `auth.env_var` joins the checked set — so
//! env-check enumerates exactly what a real launch would demand.

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

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;

    let project_path = req.project_path.ok_or_else(|| {
        HandlerError::BadRequest(
            "env-check requires a project: run inside a project directory".into(),
        )
    })?;

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
    // Provider auth: resolved via the launch path's own preflight machinery
    // (never injected), with the env var folded into the checked set.
    let provider_auth = provider_auth_report(&state, &project_path, &verified, &mut names);
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

    // Import dry-run via the shared probe (also used by `ryeos doctor`): for a
    // python tool, reproduce the launch interpreter + sys.path and attempt the
    // import (without calling `execute`), so an empty `.venv` or a
    // `ModuleNotFoundError` surfaces here rather than at first run. The probe
    // runs a bounded subprocess, so it goes off the async runtime.
    let import_report = {
        let engine = state.engine.clone();
        let probe_names = names.clone();
        tokio::task::spawn_blocking(move || {
            ryeos_app::env_probe::import_dry_run(&engine, &plan_ctx, &verified, &probe_names)
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
        "provider_auth": provider_auth,
    });
    if let (Some(obj), Some(extra)) = (response.as_object_mut(), import_report.as_object()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    Ok(response)
}

/// Resolve the provider the target would launch with and fold its auth env
/// var into `names`. Returns the `provider_auth` report block:
/// `required: false` when the target's runtime never resolves a provider;
/// `checked: false` (with the error) when provider resolution fails — a real
/// launch would fail identically, so the failure IS the finding.
fn provider_auth_report(
    state: &Arc<AppState>,
    project_path: &str,
    verified: &ryeos_engine::contracts::VerifiedItem,
    names: &mut Vec<String>,
) -> Value {
    let required_envelope_fields = match state
        .engine
        .runtimes
        .resolve_for_launch(None, &verified.resolved.kind)
    {
        Ok(runtime) => runtime.yaml.required_envelope_fields.clone(),
        Err(e) => {
            // No registered runtime for this kind → nothing resolves a
            // provider. But a kind that HAS runtimes and still fails here
            // (ambiguous defaults, broken registration) would fail a real
            // launch the same way — that failure is the finding, not
            // "nothing to check".
            let kind_has_runtimes = state
                .engine
                .runtimes
                .all()
                .any(|r| r.yaml.serves == verified.resolved.kind);
            if kind_has_runtimes {
                return serde_json::json!({
                    "checked": false,
                    "error": format!("runtime resolution for kind '{}': {e}", verified.resolved.kind),
                    "note": "a real launch fails the same way — fix the runtime registration",
                });
            }
            Vec::new()
        }
    };
    if !ryeos_executor::execution::launch::requires_provider_snapshot(&required_envelope_fields) {
        return serde_json::json!({ "checked": true, "required": false });
    }

    let project_root = std::path::PathBuf::from(project_path);
    let engine_roots = state.engine.resolution_roots(Some(project_root.clone()));
    let effective_parsers = match state
        .engine
        .effective_parser_dispatcher(Some(&project_root))
    {
        Ok(parsers) => parsers,
        Err(e) => {
            return serde_json::json!({
                "checked": false,
                "error": format!("effective parser dispatcher: {e}"),
            });
        }
    };
    let resolution = match ryeos_engine::resolution::run_resolution_pipeline(
        &verified.resolved.canonical_ref,
        &state.engine.kinds,
        &effective_parsers,
        &engine_roots,
        &state.engine.trust_store,
        &state.engine.composers,
    ) {
        Ok(resolution) => resolution,
        Err(e) => {
            return serde_json::json!({
                "checked": false,
                "error": format!("resolution pipeline: {e}"),
            });
        }
    };
    let operator_trusted_keys_dir = state.config.runtime_root().trusted_keys_dir();
    match ryeos_executor::execution::launch::resolve_provider_preflight(
        &resolution.composed,
        &engine_roots,
        &operator_trusted_keys_dir,
    ) {
        Ok(preflight) => match &preflight.env_var {
            Some(env_var) => {
                if !names.contains(env_var) {
                    names.push(env_var.clone());
                }
                serde_json::json!({
                    "checked": true,
                    "required": true,
                    "provider_id": preflight.provider_id,
                    "env_var": env_var,
                })
            }
            None => serde_json::json!({
                "checked": true,
                "required": true,
                "provider_id": preflight.provider_id,
                "env_var": null,
                "note": "provider declares no auth env var",
            }),
        },
        Err(e) => serde_json::json!({
            "checked": false,
            "error": format!("{e}"),
            "note": "a real launch fails the same way — fix the model/provider config",
        }),
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
