//! `scheduler.reauthorize` — re-pin a schedule to its owner's current grant
//! generation.
//!
//! A schedule pins the exact authorized-key grant generation admitted at
//! registration; any rewrite of that grant leaves the schedule refusing its
//! fires. The owner re-pins it here without resubmitting the spec: the signed
//! source keeps every field, including `registered_at` and the registration
//! request digest, and only the grant evidence moves to the generation that
//! authenticated this request. The class, origin and every declared
//! capability must still be covered; otherwise the request is refused with a
//! typed error and nothing is written.
//!
//! The enabled state is unchanged: a schedule the scheduler paused stays
//! paused until the owner resumes it.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_scheduler_contract::ScheduleExecutionAuthority;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schedule_id: String,
}

fn refusal(code: &str, schedule_id: &str, message: String, detail: Value) -> HandlerError {
    HandlerError::Structured {
        code: code.to_string(),
        status: 409,
        body: serde_json::json!({
            "code": code,
            "schedule_id": schedule_id,
            "message": message,
            "detail": detail,
        }),
    }
}

pub async fn handle(
    req: Request,
    ctx: crate::handler_context::HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    // The caller's own authenticated grant is the new authority.
    ctx.require_verified()?;
    let scheduler = crate::api_state::selected_scheduler(state.as_ref())
        .map_err(|e| HandlerError::Internal(format!("{e:#}")))?;
    let _mutation_guard = scheduler
        .acquire_mutation()
        .await
        .map_err(|e| HandlerError::Internal(format!("{e:#}")))?;
    let scheduler_db = scheduler.scheduler_db();
    ryeos_scheduler_contract::validate_schedule_id(&req.schedule_id)
        .map_err(|e| HandlerError::BadRequest(format!("{e:#}")))?;
    let spec = scheduler_db
        .get_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(format!("{e:#}")))?
        .ok_or(HandlerError::NotFound)?;
    ctx.require_owner(Some(spec.execution.principal_id()))?;

    let ScheduleExecutionAuthority::Authenticated {
        principal_class,
        effective_origin_site_id,
        grant_authority: pinned,
        registration_request_hash,
        ..
    } = &spec.execution.authority
    else {
        return Err(refusal(
            "schedule_not_grant_bound",
            &req.schedule_id,
            format!(
                "schedule {} is node-owned and pins no grant generation",
                req.schedule_id
            ),
            Value::Null,
        ));
    };

    // The same evidence registration seals, from this request. The
    // registration request digest is kept: the spec is not resubmitted.
    let current = super::scheduler_register::authenticated_schedule_authority(
        &ctx,
        state.as_ref(),
        registration_request_hash.clone(),
    )
    .map_err(|e| HandlerError::Forbidden(format!("{e:#}")))?;
    let ScheduleExecutionAuthority::Authenticated {
        principal_id: current_principal,
        principal_class: current_class,
        effective_origin_site_id: current_origin,
        grant_authority: current_grant,
        ..
    } = &current
    else {
        return Err(HandlerError::Internal(
            "authenticated schedule authority is not grant-bound".to_string(),
        ));
    };
    if current_principal != spec.execution.principal_id() {
        return Err(HandlerError::NotFound);
    }
    if current_class != principal_class || current_origin != effective_origin_site_id {
        return Err(refusal(
            "schedule_reauthorize_class_or_origin_changed",
            &req.schedule_id,
            format!(
                "schedule {} was registered as {principal_class:?} from {effective_origin_site_id}; the current grant is {current_class:?} from {current_origin}",
                req.schedule_id
            ),
            serde_json::json!({
                "pinned": {"principal_class": principal_class, "origin_site_id": effective_origin_site_id},
                "current": {"principal_class": current_class, "origin_site_id": current_origin},
            }),
        ));
    }
    let uncovered: Vec<&String> = spec
        .execution
        .capabilities
        .iter()
        .filter(|capability| {
            state
                .authorizer
                .authorize(
                    &ctx.scopes,
                    &ryeos_runtime::authorizer::AuthorizationPolicy::require(capability),
                )
                .is_err()
        })
        .collect();
    if !uncovered.is_empty() {
        return Err(refusal(
            "schedule_reauthorize_not_covered",
            &req.schedule_id,
            format!(
                "the current grant does not cover the capabilities schedule {} declares; register it again with a narrower capability set",
                req.schedule_id
            ),
            serde_json::json!({"uncovered_capabilities": uncovered}),
        ));
    }

    let previous_grant_hash = pinned.principal_grant_hash.clone();
    if current_grant == pinned {
        return Ok(serde_json::json!({
            "schedule_id": req.schedule_id,
            "reauthorized": false,
            "principal_grant_hash": previous_grant_hash,
            "enabled": spec.enabled,
        }));
    }
    let principal_grant_hash = current_grant.principal_grant_hash.clone();
    let rec = super::scheduler_register::rewrite_schedule_authority(&scheduler, &spec, current)
        .map_err(|e| HandlerError::Internal(format!("{e:#}")))?;
    scheduler_db
        .upsert_spec(&rec)
        .map_err(|e| HandlerError::Internal(format!("{e:#}")))?;
    scheduler.notify_reload(Some(req.schedule_id.clone()));

    Ok(serde_json::json!({
        "schedule_id": req.schedule_id,
        "reauthorized": true,
        "previous_principal_grant_hash": previous_grant_hash,
        "principal_grant_hash": principal_grant_hash,
        "enabled": rec.enabled,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/reauthorize",
    endpoint: "scheduler.reauthorize",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.scheduler/reauthorize"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
