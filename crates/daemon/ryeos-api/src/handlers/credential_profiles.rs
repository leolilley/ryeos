//! Owner-bound generic private credential-profile metadata and artifact homes.
//!
//! File names and contents remain opaque to RyeOS. A signed workload profile owns their
//! meaning and must verify them against its admitted immutable inputs.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewCredentialProfile;
use ryeos_executor::executor::ServiceAvailability;

fn require_operator<'a>(
    state: &AppState,
    ctx: &'a HandlerContext,
) -> Result<&'a str, HandlerError> {
    ryeos_app::operator_external_content::require_configured_operator(state, ctx)
        .map_err(|_| HandlerError::Forbidden("configured operator required".into()))?;
    Ok(&ctx.fingerprint)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    profile_id: String,
}

fn profile_home_id(owner: &str, profile_id: &str) -> Result<String> {
    let identity = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.credential_profile_home.v1",
        "owner_principal":owner,
        "profile_id":profile_id,
    }))?;
    Ok(format!("credential-{}", &identity[..32]))
}

async fn create(
    req: CreateRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let owner = require_operator(&state, &ctx)?.to_owned();
    let home_id = profile_home_id(&owner, &req.profile_id).map_err(internal)?;
    let _operation_guard =
        ryeos_app::hosted_operation::acquire_credential_profile_operation(&req.profile_id)
            .await
            .map_err(internal)?;
    if let Some(existing) = state
        .state_store
        .credential_profile(&req.profile_id)
        .map_err(internal)?
    {
        if existing.owner_principal != owner || existing.home_id != home_id {
            return Err(HandlerError::BadRequest(
                "credential profile identity conflicts with existing ownership".into(),
            ));
        }
        return Ok(json!({
            "profile_id":existing.profile_id,
            "home_id":existing.home_id,
            "credential_generation":existing.credential_generation,
            "state":existing.state,
            "idempotent":true,
        }));
    }
    let state_dir = state.config.runtime_state_dir();
    ryeos_app::private_artifact_home::remove_empty_orphan(&state_dir, &home_id)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    ryeos_app::private_artifact_home::create(&state_dir, &home_id, &Default::default())
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if let Err(error) = state
        .state_store
        .create_credential_profile(NewCredentialProfile {
            profile_id: &req.profile_id,
            owner_principal: &owner,
            home_id: &home_id,
        })
    {
        // RuntimeDb commits the revisioned live projection before folding it
        // into stable operational authority. If that second commit fails, the
        // next startup repairs it from the higher revision. The profile home
        // must remain with the committed projection; deleting it here would
        // turn a recoverable metadata commit gap into credential loss.
        if state
            .state_store
            .credential_profile(&req.profile_id)
            .map_err(internal)?
            .is_some()
        {
            return Err(internal(error));
        }
        let cleanup = ryeos_app::private_artifact_home::remove(&state_dir, &home_id);
        return Err(internal(match cleanup {
            Ok(_) => error,
            Err(cleanup) => error.context(format!("profile-home rollback failed: {cleanup}")),
        }));
    }
    Ok(json!({
        "profile_id": req.profile_id,
        "home_id": home_id,
        "credential_generation": 1,
        "state": "unauthenticated",
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetRequest {
    profile_id: String,
}

async fn get(
    req: GetRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let owner = require_operator(&state, &ctx)?;
    let profile = state
        .state_store
        .credential_profile(&req.profile_id)
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    ctx.require_owner(Some(&profile.owner_principal))?;
    if profile.owner_principal != owner {
        return Err(HandlerError::NotFound);
    }
    let account_digest = profile
        .sanitized_account
        .as_ref()
        .map(ryeos_state::objects::canonical_value_digest)
        .transpose()
        .map_err(internal)?;
    let mut value = serde_json::to_value(profile).map_err(internal)?;
    value
        .as_object_mut()
        .ok_or_else(|| internal("credential profile projection is not an object"))?
        .insert(
            "sanitized_account_digest".to_string(),
            account_digest.map_or(Value::Null, Value::String),
        );
    Ok(value)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeRequest {
    profile_id: String,
    credential_generation: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    profile_id: String,
    login_epoch: u64,
    expected_account_digest: String,
}

async fn confirm(
    req: ConfirmRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let owner = require_operator(&state, &ctx)?.to_owned();
    let _operation_guard =
        ryeos_app::hosted_operation::acquire_credential_profile_operation(&req.profile_id)
            .await
            .map_err(internal)?;
    let profile = state
        .state_store
        .credential_profile(&req.profile_id)
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    ctx.require_owner(Some(&profile.owner_principal))?;
    let generation = state
        .state_store
        .confirm_credential_enrollment(
            &req.profile_id,
            &owner,
            req.login_epoch,
            &req.expected_account_digest,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    Ok(json!({
        "profile_id":req.profile_id,
        "state":"active",
        "credential_generation":generation,
        "confirmed_account_digest":req.expected_account_digest,
    }))
}

async fn revoke(
    req: RevokeRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let owner = require_operator(&state, &ctx)?.to_owned();
    let initial_profile = state
        .state_store
        .credential_profile(&req.profile_id)
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    ctx.require_owner(Some(&initial_profile.owner_principal))?;
    let mut placement_thread_ids = state
        .state_store
        .dedicated_sessions_for_credential_profile(&req.profile_id)
        .map_err(internal)?
        .into_iter()
        .map(|session| session.placement_thread_id)
        .collect::<Vec<_>>();
    placement_thread_ids.sort();
    placement_thread_ids.dedup();
    let _root_operations = placement_thread_ids
        .iter()
        .map(|root| {
            ryeos_app::hosted_operation::begin_hosted_root_operation_if_appendable(
                &state.state_store,
                root,
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let _operation_guard =
        ryeos_app::hosted_operation::acquire_credential_profile_operation(&req.profile_id)
            .await
            .map_err(internal)?;
    let profile = state
        .state_store
        .credential_profile(&req.profile_id)
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    ctx.require_owner(Some(&profile.owner_principal))?;
    let generation = if matches!(profile.state.as_str(), "revoking" | "revoked") {
        if profile.credential_generation != req.credential_generation.saturating_add(1) {
            return Err(HandlerError::BadRequest(
                "credential revocation generation is stale".into(),
            ));
        }
        profile.credential_generation
    } else {
        state
            .state_store
            .revoke_credential_profile(&req.profile_id, &owner, req.credential_generation)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?
    };

    // Revocation is the first authoritative commit. Command contact checks
    // this generation, so cleanup retries cannot contact the fenced worker.
    let mut sessions = state
        .state_store
        .dedicated_sessions_for_credential_profile(&req.profile_id)
        .map_err(internal)?;
    // Prove and settle every enumerated worker before considering unattached
    // historical sessions. This preserves the conservative orphan fence
    // without making lexical session ordering affect revocation progress.
    sessions.sort_by_key(|session| session.worker_instance_id.is_none());
    for session in sessions {
        match (
            session.worker_instance_id.as_deref(),
            session.worker_boot_epoch,
        ) {
            (Some(worker_instance_id), Some(worker_boot_epoch)) => {
                let mut worker = state
                    .state_store
                    .worker_process(worker_instance_id)
                    .map_err(internal)?
                    .ok_or_else(|| internal("credential session worker projection disappeared"))?;
                if worker.state != ryeos_app::runtime_db::WorkerProcessState::Dead
                    || worker.cleanup_state != "reaped"
                {
                    let cleanup_state =
                        ryeos_app::dedicated_session_service::retire_worker_process(
                            &state,
                            &session.placement_thread_id,
                            &worker,
                        )
                        .map_err(internal)?;
                    if cleanup_state != "reaped" {
                        if worker.state != ryeos_app::runtime_db::WorkerProcessState::Dead {
                            state
                                .state_store
                                .fence_abandoned_worker_process(
                                    worker_instance_id,
                                    &session.placement_thread_id,
                                    worker_boot_epoch,
                                    "unproved",
                                )
                                .map_err(internal)?;
                        }
                        return Err(HandlerError::BadRequest(
                            "credential revocation is fenced until every owned worker reap is proved"
                                .into(),
                        ));
                    }
                    state
                        .state_store
                        .settle_worker_process(
                            worker_instance_id,
                            &session.placement_thread_id,
                            worker_boot_epoch,
                            "reaped",
                            "credential_revoked",
                        )
                        .map_err(internal)?;
                    worker = state
                        .state_store
                        .worker_process(worker_instance_id)
                        .map_err(internal)?
                        .ok_or_else(|| {
                            internal("settled credential worker projection disappeared")
                        })?;
                }
                if worker.state != ryeos_app::runtime_db::WorkerProcessState::Dead
                    || worker.cleanup_state != "reaped"
                {
                    return Err(internal("credential worker is not proved reaped"));
                }
                let refreshed = state
                    .state_store
                    .dedicated_session(&session.placement_thread_id)
                    .map_err(internal)?
                    .ok_or_else(|| internal("credential session disappeared during revoke"))?;
                if refreshed.state == "recovering" {
                    state
                        .state_store
                        .terminalize_dedicated_session(
                            &session.placement_thread_id,
                            worker_instance_id,
                            worker_boot_epoch,
                            "credential_revoked",
                        )
                        .map_err(internal)?;
                }
                let refreshed = state
                    .state_store
                    .dedicated_session(&session.placement_thread_id)
                    .map_err(internal)?
                    .ok_or_else(|| internal("credential session disappeared after revoke"))?;
                if refreshed.state == "terminal" {
                    ryeos_app::dedicated_session_service::finish_terminal_credential_cleanup(
                        &state, &refreshed,
                    )
                    .map_err(internal)?;
                }
            }
            (None, None) if session.state == "terminal" => {}
            (None, None) => {
                let current_profile = state
                    .state_store
                    .credential_profile(&req.profile_id)
                    .map_err(internal)?
                    .ok_or_else(|| internal("credential profile disappeared during revocation"))?;
                if current_profile.lock_owner.is_some() {
                    return Err(HandlerError::BadRequest(
                        "credential revocation is fenced by an unproved unattached worker start"
                            .into(),
                    ));
                }
                state
                    .state_store
                    .terminalize_unattached_dedicated_session(
                        &session.placement_thread_id,
                        "credential_revoked",
                    )
                    .map_err(internal)?;
            }
            _ => return Err(internal("dedicated session has a partial worker identity")),
        }
    }
    let final_profile = state
        .state_store
        .credential_profile(&req.profile_id)
        .map_err(internal)?
        .ok_or_else(|| internal("credential profile disappeared before revocation cleanup"))?;
    if final_profile.lock_owner.is_some() {
        return Err(HandlerError::BadRequest(
            "credential revocation cannot remove its home while worker ownership remains fenced"
                .into(),
        ));
    }
    ryeos_app::private_artifact_home::remove(&state.config.runtime_state_dir(), &profile.home_id)
        .map_err(internal)?;
    if profile.state != "revoked" {
        state
            .state_store
            .finish_credential_profile_revocation(&req.profile_id, &owner, generation)
            .map_err(internal)?;
    }
    Ok(json!({
        "profile_id": req.profile_id,
        "credential_generation": generation,
        "state": "revoked",
        "local_credentials_removed": true,
        "upstream_revocation_observed": false,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteRequest {
    profile_id: String,
    credential_generation: u64,
}

async fn delete(
    req: DeleteRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let owner = require_operator(&state, &ctx)?.to_owned();
    let _operation_guard =
        ryeos_app::hosted_operation::acquire_credential_profile_operation(&req.profile_id)
            .await
            .map_err(internal)?;
    let profile = state
        .state_store
        .credential_profile(&req.profile_id)
        .map_err(internal)?;
    let Some(profile) = profile else {
        return Ok(json!({
            "deleted":true,
            "profile_id":req.profile_id,
            "idempotent":true,
        }));
    };
    ctx.require_owner(Some(&profile.owner_principal))?;
    if profile.lock_owner.is_some() {
        return Err(HandlerError::BadRequest(
            "credential deletion requires the profile worker fence to be released".into(),
        ));
    }
    for session in state
        .state_store
        .dedicated_sessions_for_credential_profile(&req.profile_id)
        .map_err(internal)?
    {
        match session.worker_instance_id.as_deref() {
            Some(worker_instance_id) => {
                let worker = state
                    .state_store
                    .worker_process(worker_instance_id)
                    .map_err(internal)?
                    .ok_or_else(|| internal("credential worker projection disappeared"))?;
                if worker.state != ryeos_app::runtime_db::WorkerProcessState::Dead
                    || worker.cleanup_state != "reaped"
                {
                    return Err(HandlerError::BadRequest(
                        "credential deletion requires proved reap of every owned worker".into(),
                    ));
                }
            }
            None if session.worker_boot_epoch.is_none() => {}
            None => return Err(internal("credential session has a partial worker identity")),
        }
    }
    let deleting_generation = if profile.state == "deleting" {
        if profile.credential_generation != req.credential_generation.saturating_add(1) {
            return Err(HandlerError::BadRequest(
                "credential deletion generation is stale".into(),
            ));
        }
        profile.credential_generation
    } else {
        state
            .state_store
            .begin_credential_profile_deletion(&req.profile_id, &owner, req.credential_generation)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?
    };
    ryeos_app::private_artifact_home::remove(&state.config.runtime_state_dir(), &profile.home_id)
        .map_err(internal)?;
    state
        .state_store
        .finish_credential_profile_deletion(&req.profile_id, &owner, deleting_generation)
        .map_err(internal)?;
    Ok(json!({"deleted": true, "profile_id": req.profile_id}))
}

fn internal(error: impl std::fmt::Display) -> HandlerError {
    HandlerError::Internal(error.to_string())
}

pub const CREATE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:credential-profiles/create",
    endpoint: "credential-profiles.create",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.credential-profiles/create"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: CreateRequest = crate::handler_error::parse_request(params)?;
            create(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const GET_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:credential-profiles/get",
    endpoint: "credential-profiles.get",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.credential-profiles/get"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: GetRequest = crate::handler_error::parse_request(params)?;
            get(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const REVOKE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:credential-profiles/revoke",
    endpoint: "credential-profiles.revoke",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.credential-profiles/revoke"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: RevokeRequest = crate::handler_error::parse_request(params)?;
            revoke(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const CONFIRM_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:credential-profiles/confirm",
    endpoint: "credential-profiles.confirm",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.credential-profiles/confirm"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ConfirmRequest = crate::handler_error::parse_request(params)?;
            confirm(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const DELETE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:credential-profiles/delete",
    endpoint: "credential-profiles.delete",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.credential-profiles/delete"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: DeleteRequest = crate::handler_error::parse_request(params)?;
            delete(req, ctx, state).await.map_err(Into::into)
        })
    },
};
