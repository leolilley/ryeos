//! `bundle.smoke` — execute a bundle's declared smoke list against its
//! source tree, with runtime state isolated under a temporary state root.
//!
//! The smoke list is signed manifest data (`smoke:` in manifest.source.yaml,
//! see `ryeos_bundle::manifest::SmokeDecl`). Each entry dispatches as a
//! normal waited thread — same resolution pipeline, trust verification,
//! capability minting, and braid visibility as any operator execution — with
//! the project anchored at the bundle source and a `state_root` override
//! keeping every runtime write out of the authored tree.
//!
//! Failure surface: bundle preflight (signature/contract verification, the
//! same pass `ryeos doctor` wraps) fails the whole run before any thread
//! launches; per-entry dispatch failures are collected, never short-circuit.
//! The report always carries the state root so failed runs can be inspected;
//! it is removed only when every entry passed and `keep_state` is unset.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_bundle::manifest::{validate_smoke_decls, BundleManifestSource, SmokeDecl};
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Bundle source directory containing `.ai/` (absolute).
    pub source: String,
    /// Keep the temporary state root even when every entry passes.
    #[serde(default)]
    pub keep_state: bool,
}

/// Outcome of one smoke entry, flattened for the report.
struct EntryOutcome {
    label: String,
    item_ref: String,
    status: &'static str,
    thread_id: Option<String>,
    duration_ms: u64,
    error: Option<String>,
}

impl EntryOutcome {
    fn to_json(&self) -> Value {
        let mut entry = json!({
            "label": self.label,
            "ref": self.item_ref,
            "status": self.status,
            "duration_ms": self.duration_ms,
        });
        let obj = entry.as_object_mut().expect("literal object");
        if let Some(tid) = &self.thread_id {
            obj.insert("thread_id".to_string(), json!(tid));
        }
        if let Some(err) = &self.error {
            obj.insert("error".to_string(), json!(err));
        }
        entry
    }
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;

    let source = PathBuf::from(&req.source);
    if !source.is_absolute() {
        return Err(HandlerError::BadRequest(format!(
            "source must be an absolute path, got '{}'",
            req.source
        )));
    }
    let ai_dir = source.join(".ai");
    if !ai_dir.is_dir() {
        return Err(HandlerError::BadRequest(format!(
            "'{}' is not a bundle source (no .ai/ directory)",
            source.display()
        )));
    }

    // Smoke declarations come from the SOURCE manifest: smoke runs target the
    // tree being authored, which may deliberately differ from any installed
    // (signed) manifest for the same bundle.
    let manifest_path = ai_dir.join("manifest.source.yaml");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| HandlerError::BadRequest(format!("read {}: {e}", manifest_path.display())))?;
    let manifest: BundleManifestSource = serde_yaml::from_str(&raw)
        .map_err(|e| HandlerError::BadRequest(format!("parse {}: {e}", manifest_path.display())))?;
    validate_smoke_decls(&manifest.smoke)
        .map_err(|e| HandlerError::BadRequest(format!("invalid smoke declaration: {e}")))?;

    if manifest.smoke.is_empty() {
        return Ok(json!({
            "source": req.source,
            "success": true,
            "entries": [],
            "note": "no smoke entries declared (add a `smoke:` list to manifest.source.yaml)",
        }));
    }

    // Bundle preflight — the same verification pass `ryeos doctor` wraps.
    // A blocking failure here means threads would fail at resolution anyway;
    // fail fast with the real reason instead of N confusing thread errors.
    let dependency_roots: Vec<PathBuf> = state
        .node_config
        .bundles
        .iter()
        .map(|record| record.path.clone())
        .collect();
    let operator_config_root =
        ryeos_engine::roots::RuntimeRoot::new(state.config.app_root.clone()).config();
    let preflight_warnings =
        match ryeos_bundle::preflight::preflight_verify_bundle_report_in_context(
            &source,
            &dependency_roots,
            &operator_config_root,
            std::sync::Arc::clone(&state.isolation),
        ) {
            Ok(report) => report
                .warnings
                .iter()
                .map(|w| format!("{w:?}"))
                .collect::<Vec<_>>(),
            Err(e) => {
                return Err(HandlerError::BadRequest(format!(
                    "bundle preflight failed (fix before smoking): {e:#}"
                )));
            }
        };

    // Temporary state root, outside the source by construction. Created here
    // so per-entry dispatches inherit an existing directory. The pid +
    // process-wide counter make concurrent smoke runs collision-free even
    // within one millisecond.
    static SMOKE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let state_root = std::env::temp_dir().join(format!(
        "ryeos-smoke-{}-{}-{}",
        lillux::time::timestamp_millis(),
        std::process::id(),
        SMOKE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&state_root).map_err(|e| {
        HandlerError::Internal(format!("create state root {}: {e}", state_root.display()))
    })?;

    let mut outcomes: Vec<EntryOutcome> = Vec::with_capacity(manifest.smoke.len());
    for decl in &manifest.smoke {
        outcomes.push(run_entry(decl, &source, &state_root, &ctx, &state).await);
    }

    let failed = outcomes.iter().filter(|o| o.status != "passed").count();
    let success = failed == 0;

    // Keep the state root whenever it may still be wanted: explicit request
    // or anything failed (the transcripts under it are the diagnostics).
    let keep = req.keep_state || !success;
    if !keep {
        if let Err(e) = std::fs::remove_dir_all(&state_root) {
            tracing::warn!(
                state_root = %state_root.display(),
                error = %e,
                "smoke state-root cleanup failed"
            );
        }
    }

    Ok(json!({
        "source": req.source,
        "success": success,
        "passed": outcomes.len() - failed,
        "failed": failed,
        "entries": outcomes.iter().map(EntryOutcome::to_json).collect::<Vec<_>>(),
        "state_root": state_root,
        "state_root_kept": keep,
        "preflight_warnings": preflight_warnings,
    }))
}

/// Dispatch one smoke entry as a normal waited thread against the bundle
/// source with the state-root override. Never propagates: every failure mode
/// becomes an `EntryOutcome` so remaining entries still run.
async fn run_entry(
    decl: &SmokeDecl,
    source: &std::path::Path,
    state_root: &std::path::Path,
    ctx: &HandlerContext,
    state: &Arc<AppState>,
) -> EntryOutcome {
    let started = lillux::time::timestamp_millis();
    let mut outcome = EntryOutcome {
        label: decl.label().to_string(),
        item_ref: decl.item_ref.clone(),
        status: "failed",
        thread_id: None,
        duration_ms: 0,
        error: None,
    };

    let params = if decl.inputs.is_null() {
        json!({})
    } else if decl.inputs.is_object() {
        decl.inputs.clone()
    } else {
        outcome.error = Some(format!(
            "smoke inputs for '{}' must be an object, got {}",
            decl.label(),
            decl.inputs
        ));
        return outcome;
    };

    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};
    let site_id = state.threads.site_id().to_string();
    let origin_site_id = ctx.execution_origin(&site_id);
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: ctx.fingerprint.clone(),
            scopes: ctx.scopes.clone(),
        }),
        project_context: ProjectContext::LocalPath {
            path: source.to_path_buf(),
        },
        current_site_id: site_id.clone(),
        origin_site_id,
        execution_hints: Default::default(),
        validate_only: false,
    };
    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: ctx.fingerprint.clone(),
        caller_scopes: ctx.scopes.clone(),
        engine: state.engine.clone(),
        plan_ctx,
        requested_call: None,
    };
    if let Err(error) =
        ryeos_app::execution_policy::authorize_standard_local_live_execution(&ctx.scopes)
    {
        outcome.error = Some(format!("authorize smoke execution: {error:#}"));
        return outcome;
    }
    let resolved_authority =
        match ryeos_app::execution_policy::resolve_standard_local_live_authority(
            source,
            ctx.scopes.clone(),
            &state.isolation,
        ) {
            Ok(authority) => authority,
            Err(error) => {
                outcome.error = Some(format!("resolve smoke execution authority: {error:#}"));
                return outcome;
            }
        };
    let provenance = match ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
        source.to_path_buf(),
        state.engine.clone(),
        resolved_authority.project,
    ) {
        Ok(provenance) => provenance.with_state_root(Some(state_root.to_path_buf())),
        Err(error) => {
            outcome.error = Some(format!("construct smoke execution provenance: {error:#}"));
            return outcome;
        }
    };
    let kind = decl.item_ref.split(':').next().unwrap_or("");
    let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
        launch_mode: "wait",
        target_site_id: None,
        validate_only: false,
        params,
        ref_bindings: decl.ref_bindings.clone(),
        acting_principal: ctx.fingerprint.as_str(),
        project_path: source,
        provenance,
        lifecycle_authority: resolved_authority.lifecycle,
        launch_timings: None,
        original_root_kind: kind,
        pre_minted_thread_id: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        root_admission: None,
        parent_execution_context: None,
    };

    let dispatch =
        ryeos_executor::dispatch::dispatch(&decl.item_ref, &dispatch_req, &exec_ctx, state);
    let result = match decl.timeout_secs {
        Some(secs) => {
            match tokio::time::timeout(std::time::Duration::from_secs(secs), dispatch).await {
                Ok(r) => r,
                Err(_) => {
                    // The abandoned dispatch's thread keeps running daemon-side;
                    // report it as such rather than pretending it stopped.
                    outcome.status = "timeout";
                    outcome.duration_ms =
                        lillux::time::timestamp_millis().saturating_sub(started) as u64;
                    outcome.error = Some(format!(
                        "no result within {secs}s; the thread may still be running"
                    ));
                    return outcome;
                }
            }
        }
        None => dispatch.await,
    };
    outcome.duration_ms = lillux::time::timestamp_millis().saturating_sub(started) as u64;

    match result {
        Ok(value) => {
            // Inline dispatch resolves Ok for finalized threads regardless of
            // terminal status; the envelope is `{thread: <ThreadDetail>,
            // result: <runtime result>}`. A smoke entry passes ONLY on a
            // `completed` thread whose runtime result doesn't claim failure —
            // any unrecognized shape fails loudly rather than passing.
            let thread = value.get("thread").cloned().unwrap_or(Value::Null);
            outcome.thread_id = thread
                .get("thread_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let status = thread.get("status").and_then(Value::as_str).unwrap_or("");
            let result_claims_failure = value
                .get("result")
                .and_then(|r| r.get("success"))
                .and_then(Value::as_bool)
                == Some(false);
            if status == "completed" && !result_claims_failure {
                outcome.status = "passed";
            } else {
                outcome.error = Some(
                    value
                        .get("result")
                        .filter(|r| !r.is_null())
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| format!("thread finished with status '{status}'")),
                );
            }
        }
        Err(e) => {
            outcome.error = Some(format!("{e:#}"));
        }
    }
    outcome
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/smoke",
    endpoint: "bundle.smoke",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle/smoke"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_rejects_unknown_fields() {
        let result = serde_json::from_value::<Request>(json!({
            "source": "/b",
            "state_root": "/tmp/x",
        }));
        assert!(result.is_err(), "unknown fields must be rejected");
    }

    #[test]
    fn request_defaults_keep_state_off() {
        let req: Request = serde_json::from_value(json!({ "source": "/b" })).unwrap();
        assert!(!req.keep_state);
    }

    #[test]
    fn entry_outcome_json_carries_optionals_only_when_set() {
        let bare = EntryOutcome {
            label: "health".into(),
            item_ref: "tool:demo/health".into(),
            status: "passed",
            thread_id: None,
            duration_ms: 12,
            error: None,
        };
        let v = bare.to_json();
        assert!(v.get("thread_id").is_none());
        assert!(v.get("error").is_none());
        assert_eq!(v["status"], "passed");

        let failed = EntryOutcome {
            thread_id: Some("T-1".into()),
            error: Some("boom".into()),
            status: "failed",
            ..bare
        };
        let v = failed.to_json();
        assert_eq!(v["thread_id"], "T-1");
        assert_eq!(v["error"], "boom");
    }
}
