//! Launch invoker — spawns a background dispatch and returns `Accepted`.
//!
//! Used by `launch` mode routes (webhooks, async triggers). The invoker
//! mints a thread ID, spawns the dispatch in a background task, and returns
//! `RouteInvocationResult::Accepted` only after authoritative launch handoff.
//! The caller (mode) frames it as HTTP 202.

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::{
    authenticated_execution_origin, CompiledRouteInvocation, PrincipalPolicy,
    RouteInvocationContext, RouteInvocationContract, RouteInvocationOutput, RouteInvocationResult,
};

pub struct CompiledLaunchInvocation;

static LAUNCH_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Accepted,
    principal: PrincipalPolicy::Optional,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledLaunchInvocation {
    fn contract(&self) -> &'static RouteInvocationContract {
        &LAUNCH_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        // Extract prepared launch parameters from input.
        let item_ref_str = ctx
            .input
            .get("item_ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                RouteDispatchError::Internal("launch invoker: missing item_ref in input".into())
            })?;

        let project_path_str = ctx
            .input
            .get("project_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                RouteDispatchError::Internal("launch invoker: missing project_path in input".into())
            })?;
        let ref_bindings = serde_json::from_value::<std::collections::BTreeMap<String, String>>(
            ctx.input
                .get("ref_bindings")
                .ok_or_else(|| {
                    RouteDispatchError::Internal(
                        "launch invoker: missing ref_bindings in input".into(),
                    )
                })?
                .clone(),
        )
        .map_err(|error| {
            RouteDispatchError::Internal(format!(
                "launch invoker: invalid ref_bindings in input: {error}"
            ))
        })?;

        let parameters = ctx
            .input
            .get("parameters")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let principal_id = ctx
            .principal
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();

        let principal_scopes = ctx
            .principal
            .as_ref()
            .map(|p| p.scopes.clone())
            .unwrap_or_default();
        let execution_origin_site_id =
            authenticated_execution_origin(ctx.principal.as_ref(), ctx.state.threads.site_id());

        // Parse and validate canonical ref.
        let item_ref =
            crate::routes::parsed_ref::ParsedItemRef::parse(item_ref_str).map_err(|e| {
                RouteDispatchError::BadRequest(format!(
                    "invalid item_ref '{}': {}",
                    item_ref_str, e
                ))
            })?;

        let project_path = crate::routes::abs_path::AbsolutePathBuf::try_from_str(project_path_str)
            .map_err(|e| RouteDispatchError::BadRequest(format!("project_path: {e}")))?;

        let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
        ryeos_app::execution_policy::authorize_standard_local_live_execution(&principal_scopes)
            .map_err(|error| RouteDispatchError::Forbidden(error.to_string()))?;
        let mut project_ctx = ryeos_executor::execution::project_source::resolve_project_context(
            &ctx.state,
            &ryeos_executor::execution::project_source::ProjectSource::LiveFs,
            project_path.as_path(),
            &principal_id,
            &format!("route-{thread_id}"),
            ryeos_executor::execution::project_source::PinnedContextRealization::Cow,
        )
        .map_err(|error| RouteDispatchError::BadRequest(format!("capture project: {error}")))?;
        let effective_project = crate::routes::abs_path::AbsolutePathBuf::try_from_str(
            project_ctx.effective_path.to_str().ok_or_else(|| {
                RouteDispatchError::Internal("captured project path is not UTF-8".to_string())
            })?,
        )
        .map_err(|error| RouteDispatchError::Internal(format!("captured project path: {error}")))?;

        let preflight = crate::routes::launch::preflight_dispatch_launch(
            &ctx.state,
            &item_ref,
            &project_ctx,
            &parameters,
            &ref_bindings,
            &principal_id,
            &principal_scopes,
            &execution_origin_site_id,
            None,
            "wait",
            false,
            None,
            None,
        )
        .map_err(|error| {
            RouteDispatchError::BadRequest(format!("launch admission failed: {error}"))
        })?;
        if !preflight.class.persists_pre_minted_root() {
            return Err(RouteDispatchError::BadRequest(
                "background launch requires execution that persists a pre-minted thread root"
                    .to_string(),
            ));
        }
        let root_admission = preflight.root_admission.ok_or_else(|| {
            RouteDispatchError::Internal(
                "threaded dispatch preflight returned no root admission".to_string(),
            )
        })?;
        let resolved_authority =
            ryeos_app::execution_policy::resolve_standard_local_live_authority(
                &project_ctx.effective_path,
                principal_scopes.clone(),
                &ctx.state.isolation,
            )
            .map_err(|error| RouteDispatchError::BadRequest(error.to_string()))?;
        let launch_options = crate::routes::launch::DispatchLaunchOptions::admitted(
            root_admission,
            effective_project.as_path(),
            ref_bindings,
            resolved_authority.lifecycle,
        )
        .map_err(|error| {
            RouteDispatchError::Internal(format!(
                "validated launch contract rejected at dispatch boundary: {error:#}"
            ))
        })?
        .retain_captured_generation(project_ctx.take_captured_generation());

        let route_id: String = ctx.route_id.to_string();

        let span = tracing::info_span!(
            "launch_invoker",
            route_id = route_id.as_str(),
            thread_id = thread_id.as_str(),
            item_ref_kind = item_ref.kind(),
        );

        let launch_provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
            project_ctx.effective_path.clone(),
            project_ctx.request_engine.clone(),
            resolved_authority.project,
        )
        .map_err(|error| RouteDispatchError::BadRequest(error.to_string()))?;
        let (mut handle, ready) = crate::routes::launch::spawn_dispatch_launch_with_handoff(
            &ctx.state,
            item_ref,
            parameters,
            principal_id.clone(),
            principal_scopes,
            thread_id.clone(),
            launch_provenance,
            launch_options,
        );

        let ready_thread_id = tokio::select! {
            biased;
            readiness = ready => match readiness {
                Ok(Ok(ready_thread_id)) => ready_thread_id,
                Ok(Err(failure)) => {
                    return Err(RouteDispatchError::Structured {
                        code: failure.code,
                        status: failure.status,
                        body: failure.body,
                    });
                }
                Err(_) => {
                    return Err(launch_task_error(handle.await));
                }
            },
            result = &mut handle => {
                return Err(launch_task_error(result));
            }
        };
        if ready_thread_id != thread_id {
            return Err(RouteDispatchError::Internal(
                "authoritative handoff returned a different thread identity".to_string(),
            ));
        }

        // Detached watcher: the ID is now safe to expose; continue observing
        // the launched task after the response is returned.
        let watch_route_id = route_id;
        let watch_thread_id = thread_id.clone();
        tokio::spawn(async move {
            let _guard = span.enter();
            match handle.await {
                Ok(Ok(())) => {
                    tracing::debug!(
                        route_id = %watch_route_id,
                        thread_id = %watch_thread_id,
                        "launch invoker background dispatch completed"
                    );
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        route_id = %watch_route_id,
                        thread_id = %watch_thread_id,
                        code = %e.code(),
                        error = %e,
                        "launch invoker background dispatch failed"
                    );
                }
                Err(join_err) => {
                    tracing::error!(
                        route_id = %watch_route_id,
                        thread_id = %watch_thread_id,
                        error = %join_err,
                        "launch invoker background dispatch panicked"
                    );
                }
            }
        });

        Ok(RouteInvocationResult::Accepted {
            thread_id: thread_id.to_string(),
        })
    }
}

fn launch_task_error(
    result: Result<Result<(), crate::routes::launch::LaunchSpawnError>, tokio::task::JoinError>,
) -> RouteDispatchError {
    match result {
        Ok(Err(crate::routes::launch::LaunchSpawnError::Dispatch(error))) => {
            RouteDispatchError::Structured {
                code: error.code().to_string(),
                status: error.http_status().as_u16(),
                body: ryeos_executor::structured_error::StructuredErrorPayload::from(&error)
                    .to_value(),
            }
        }
        Ok(Err(crate::routes::launch::LaunchSpawnError::InvalidRef { ref_str, reason })) => {
            RouteDispatchError::BadRequest(format!("invalid item_ref '{ref_str}': {reason}"))
        }
        Ok(Ok(())) => RouteDispatchError::Internal(
            "launch completed without authoritative handoff".to_string(),
        ),
        Err(error) => RouteDispatchError::Internal(format!("launch task failed: {error}")),
    }
}
