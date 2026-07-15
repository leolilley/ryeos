//! Launch invoker — spawns a background dispatch and returns `Accepted`.
//!
//! Used by `launch` mode routes (webhooks, async triggers). The invoker
//! mints a thread ID, spawns `spawn_dispatch_launch` in a background task,
//! and returns `RouteInvocationResult::Accepted` immediately. The caller
//! (mode) frames it as HTTP 202.

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContext, RouteInvocationContract,
    RouteInvocationOutput, RouteInvocationResult,
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

        let preflight = crate::routes::launch::preflight_dispatch_launch(
            &ctx.state,
            &item_ref,
            &project_path,
            &parameters,
            &principal_id,
            &principal_scopes,
            None,
            "inline",
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
        let launch_options = crate::routes::launch::DispatchLaunchOptions::admitted(root_admission)
            .map_err(|error| {
                RouteDispatchError::Internal(format!(
                    "validated launch contract rejected at dispatch boundary: {error:#}"
                ))
            })?;

        let thread_id = ryeos_app::thread_lifecycle::new_thread_id();

        let route_id: String = ctx.route_id.to_string();

        let span = tracing::info_span!(
            "launch_invoker",
            route_id = route_id.as_str(),
            thread_id = thread_id.as_str(),
            item_ref_kind = item_ref.kind(),
        );

        let handle = crate::routes::launch::spawn_dispatch_launch(
            &ctx.state,
            item_ref,
            parameters,
            principal_id.clone(),
            principal_scopes,
            thread_id.clone(),
            launch_options,
        );

        // Detached watcher: await the handle and log outcome.
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
