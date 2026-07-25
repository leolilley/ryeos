//! Generic compiled invoker for service canonical refs.
//!
//! A single `CompiledServiceInvocation` executes any verified service item
//! through the shared service executor. No per-service hand-written invoker
//! structs — the handlers all go through the same admission, recording, and
//! terminal-settlement boundary as `/execute`.

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContext, RouteInvocationContract,
    RouteInvocationOutput, RouteInvocationResult,
};

/// Generic invoker for `service:` canonical refs.
///
/// At compile time the endpoint is validated against the service descriptor
/// list. At runtime the endpoint is looked up in the `ServiceRegistry` and
/// called with the interpolated input.
pub struct CompiledServiceInvocation {
    /// Canonical verified service subject.
    pub service_ref: String,
    /// Service endpoint string (e.g., `"threads.get"`).
    /// Retained for compile/runtime catalog drift diagnostics.
    pub endpoint: String,
}

static SERVICE_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Json,
    principal: PrincipalPolicy::Optional,
};

fn route_handler_context(
    principal: Option<&crate::routes::invocation::RoutePrincipal>,
) -> crate::handler_context::HandlerContext {
    principal
        .map(|principal| {
            crate::handler_context::HandlerContext::new_with_origin(
                principal.id.clone(),
                principal.scopes.clone(),
                principal.verified,
                principal.authenticated_origin_site_id.clone(),
            )
        })
        .unwrap_or_else(crate::handler_context::HandlerContext::anonymous)
}

fn recorded_route_usage<'a>(
    record_thread: bool,
    principal: Option<&'a crate::routes::invocation::RoutePrincipal>,
    route_id: &str,
) -> Result<(Option<ryeos_state::UsageSubject>, Option<&'a str>), RouteDispatchError> {
    if !record_thread {
        return Ok((None, None));
    }
    let principal = principal
        .filter(|principal| !principal.id.is_empty())
        .ok_or(RouteDispatchError::Unauthorized)?;
    Ok((
        Some(ryeos_state::UsageSubject {
            namespace: "route".to_string(),
            subject: route_id.to_string(),
        }),
        Some(principal.id.as_str()),
    ))
}

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledServiceInvocation {
    fn contract(&self) -> &'static RouteInvocationContract {
        &SERVICE_CONTRACT
    }

    async fn invoke(
        &self,
        inv_ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};

        let (principal_id, principal_scopes) = inv_ctx
            .principal
            .as_ref()
            .map(|principal| (principal.id.clone(), principal.scopes.clone()))
            .unwrap_or_default();
        let site_id = inv_ctx.state.threads.site_id().to_string();
        let origin_site_id = crate::routes::invocation::authenticated_execution_origin(
            inv_ctx.principal.as_ref(),
            &site_id,
        );
        let plan_ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: principal_id.clone(),
                scopes: principal_scopes.clone(),
            }),
            project_context: ProjectContext::None,
            current_site_id: site_id,
            origin_site_id,
            execution_hints: Default::default(),
            validate_only: false,
        };
        let exec_ctx = ryeos_executor::executor::ExecutionContext {
            principal_fingerprint: principal_id,
            caller_scopes: principal_scopes,
            engine: inv_ctx.state.engine.clone(),
            plan_ctx,
            requested_call: None,
        };
        let handler_context = route_handler_context(inv_ctx.principal.as_ref());
        let verified = ryeos_executor::executor::resolve_and_verify(
            &exec_ctx.engine,
            &exec_ctx.plan_ctx,
            &self.service_ref,
            Some("service"),
        )
        .map_err(|error| {
            RouteDispatchError::Internal(format!(
                "compiled service '{}' failed resolution/verification: {error}",
                self.service_ref
            ))
        })?;
        let verified_endpoint =
            ryeos_app::service_registry::extract_endpoint(&verified.resolved.metadata.extra)
                .map_err(|error| {
                    RouteDispatchError::Internal(format!(
                        "compiled service '{}' has invalid verified endpoint metadata: {error}",
                        self.service_ref
                    ))
                })?;
        if verified_endpoint != self.endpoint {
            return Err(RouteDispatchError::Internal(format!(
                "compiled service endpoint '{}' differs from verified endpoint '{}'",
                self.endpoint, verified_endpoint
            )));
        }
        if ryeos_app::service_registry::extract_ui_dispatch(&verified.resolved.metadata.extra)
            .map_err(|error| {
                RouteDispatchError::Internal(format!(
                    "compiled service '{}' has invalid verified dispatch metadata: {error}",
                    self.service_ref
                ))
            })?
            == ryeos_app::service_registry::UiDispatchMode::SessionLocal
        {
            return Err(RouteDispatchError::Internal(format!(
                "compiled node route cannot invoke session-local service '{}'",
                self.service_ref
            )));
        }
        let required_caps =
            ryeos_app::service_registry::extract_required_caps(&verified.resolved.metadata.extra);
        if !required_caps.is_empty()
            && !inv_ctx
                .principal
                .as_ref()
                .is_some_and(|principal| principal.verified)
        {
            return Err(RouteDispatchError::Unauthorized);
        }
        let record_thread =
            ryeos_app::service_registry::extract_record_thread(&verified.resolved.metadata.extra)
                .map_err(|error| {
                RouteDispatchError::Internal(format!(
                    "compiled service '{}' has invalid recording metadata: {error}",
                    self.service_ref
                ))
            })?;
        let (usage_subject, usage_subject_asserted_by) =
            recorded_route_usage(record_thread, inv_ctx.principal.as_ref(), &inv_ctx.route_id)?;

        let recorded_invocation_id =
            record_thread.then(ryeos_executor::executor::mint_service_invocation_id);
        let execution = ryeos_executor::executor::execute_service_verified(
            verified,
            &self.service_ref,
            inv_ctx.input,
            ryeos_executor::executor::ExecutionMode::Live,
            &exec_ctx,
            &inv_ctx.state,
            ryeos_executor::executor::ServiceRecordingContext {
                authority_source:
                    ryeos_executor::executor::ServiceRecordingAuthoritySource::ExplicitProjectless,
                usage_subject: usage_subject.as_ref(),
                usage_subject_asserted_by,
            },
            recorded_invocation_id.as_deref(),
            Some(handler_context),
        )
        .await;
        let result = match execution {
            Ok(result) => result,
            Err(error) => {
                let durable_thread_id = if let Some(thread_id) = recorded_invocation_id.as_ref() {
                    match inv_ctx
                        .state
                        .state_store
                        .get_authoritative_root_thread_snapshot(thread_id)
                    {
                        Ok(Some(_)) => Some(thread_id.clone()),
                        Ok(None) => None,
                        Err(read_error) => {
                            return Err(RouteDispatchError::Internal(format!(
                                "compiled service '{}' failed and its recorded thread identity could not be verified: {read_error}",
                                self.service_ref
                            )));
                        }
                    }
                } else {
                    None
                };
                let dispatch_error = error
                    .downcast::<ryeos_executor::dispatch_error::DispatchError>()
                    .unwrap_or_else(ryeos_executor::dispatch_error::DispatchError::Internal);
                return Err(RouteDispatchError::Structured {
                    code: dispatch_error.code().to_owned(),
                    status: dispatch_error.http_status().as_u16(),
                    body: ryeos_executor::structured_error::dispatch_error_value(&dispatch_error),
                    thread_id: durable_thread_id,
                });
            }
        };

        debug_assert_eq!(result.endpoint, self.endpoint);
        Ok(RouteInvocationResult::Json {
            value: result.value,
            thread_id: result.recorded.then_some(result.invocation_id),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn route_handler_context_preserves_unverified_principal() {
        let principal = crate::routes::invocation::RoutePrincipal {
            id: "route:anonymous".to_string(),
            scopes: vec!["public.read".to_string()],
            verifier_key: "none",
            verified: false,
            authenticated_origin_site_id: None,
            metadata: BTreeMap::new(),
        };

        let context = route_handler_context(Some(&principal));
        assert_eq!(context.fingerprint, principal.id);
        assert_eq!(context.scopes, principal.scopes);
        assert!(!context.verified);
        assert_eq!(context.authenticated_origin_site_id, None);
    }

    #[test]
    fn route_handler_context_preserves_verified_remote_origin() {
        let principal = crate::routes::invocation::RoutePrincipal {
            id: "fp:remote".to_string(),
            scopes: vec!["threads.read".to_string()],
            verifier_key: "ryeos_signed",
            verified: true,
            authenticated_origin_site_id: Some("site:remote".to_string()),
            metadata: BTreeMap::new(),
        };

        let context = route_handler_context(Some(&principal));
        assert!(context.verified);
        assert_eq!(
            context.authenticated_origin_site_id,
            principal.authenticated_origin_site_id
        );
    }

    #[test]
    fn recorded_route_requires_a_nonempty_principal_and_attributes_the_route() {
        assert!(matches!(
            recorded_route_usage(true, None, "route:test"),
            Err(RouteDispatchError::Unauthorized)
        ));
        let empty = crate::routes::invocation::RoutePrincipal::anonymous(String::new(), "none");
        assert!(matches!(
            recorded_route_usage(true, Some(&empty), "route:test"),
            Err(RouteDispatchError::Unauthorized)
        ));

        let principal = crate::routes::invocation::RoutePrincipal::anonymous(
            "route:public".to_string(),
            "none",
        );
        let (subject, asserted_by) =
            recorded_route_usage(true, Some(&principal), "route:test").unwrap();
        assert_eq!(
            subject,
            Some(ryeos_state::UsageSubject {
                namespace: "route".to_string(),
                subject: "route:test".to_string(),
            })
        );
        assert_eq!(asserted_by, Some("route:public"));
        assert_eq!(
            recorded_route_usage(false, None, "route:test").unwrap(),
            (None, None)
        );
    }
}
