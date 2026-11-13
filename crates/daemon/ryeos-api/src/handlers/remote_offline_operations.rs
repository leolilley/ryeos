//! `remote/offline-operations-{submit,facts,status}` — the origin operator's
//! offline operations delivered to a remote node through this node.
//!
//! The operator signs an offline-operation request with its own key, away from
//! the daemon. This node forwards it to the remote's `offline-operations`
//! routes under the delegation the operator stored for that remote: this
//! node's key signs and co-signs the call, the operator's signed delegation
//! rides with it, and the remote admits the operator as the principal. The
//! request's input files travel in the same call, so nothing is written onto
//! the remote outside its own inbox. This node never holds the operator's key
//! and never signs the request itself; the remote verifies it exactly as its
//! startup applier will.
//!
//! The remote's effective scopes are its grant to the operator intersected
//! with the delegation, so a delegation that does not lend the remote route's
//! capability is refused here, before the call, with the scope it must name.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::handlers::offline_operations::StopWhen;
use crate::registry::ServiceDescriptor;
use crate::remote::client::{RemoteClient, RemoteHttpError, delegator_fingerprint};
use crate::remote::config;
use ryeos_app::handler_error::delegation_handler_error;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node::offline_operation::contract::SignedRequest;
use ryeos_node_runtime::remote_delegation::DelegationError;

/// The remote's capability for each forwarded route. The operator's
/// delegation for the remote must lend it, and the remote's grant to the
/// operator must name it.
pub const REMOTE_SUBMIT_CAP: &str = "ryeos.execute.service.offline-operations/submit";
pub const REMOTE_FACTS_CAP: &str = "ryeos.execute.service.offline-operations/facts";
pub const REMOTE_STATUS_CAP: &str = "ryeos.execute.service.offline-operations/status";

/// The operator's stored delegation does not lend the remote route's
/// capability.
pub const DELEGATION_SCOPE_MISSING: &str = "remote_delegation_scope_missing";
/// The remote admitted the operator but its grant does not name the route's
/// capability.
pub const GRANT_SCOPE_MISSING: &str = "remote_grant_scope_missing";
/// The request is signed by another key than the caller's.
pub const SIGNER_NOT_CALLER: &str = "offline_operation_signer_not_caller";
/// The remote verified the request and refused it.
pub const REMOTE_REFUSED: &str = "remote_offline_operation_refused";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub remote: String,
    pub request: SignedRequest,
    /// Input bytes by their SHA-256, base64 encoded.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    /// `"now"` asks the remote to stop once the request is queued.
    #[serde(default)]
    pub stop: Option<StopWhen>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub remote: String,
}

fn load_remote(state: &AppState, name: &str) -> HandlerResult<config::RemoteConfig> {
    let remotes = config::load_remotes(&state.config.app_root).map_err(|error| {
        config::typed_load_error_or(error, |error| {
            HandlerError::Internal(format!("load remotes: {error:#}"))
        })
    })?;
    config::get_remote(&remotes, name)
        .map_err(|error| HandlerError::BadRequest(format!("remote '{name}': {error:#}")))
}

fn scope_missing(remote: &str, required: &str) -> HandlerError {
    HandlerError::Structured {
        code: DELEGATION_SCOPE_MISSING.to_owned(),
        status: 403,
        body: json!({
            "code": DELEGATION_SCOPE_MISSING,
            "error": format!(
                "your delegation for remote `{remote}` does not lend `{required}`, and the remote \
                 acts for you only within what the delegation lends; sign one that does: \
                 `ryeos remote delegate --remote {remote} --scopes {REMOTE_SUBMIT_CAP},{REMOTE_FACTS_CAP},{REMOTE_STATUS_CAP}`"
            ),
            "remote": remote,
            "required_scope": required,
        }),
    }
}

/// A delegated client for `remote`, acting for the caller, whose stored
/// delegation lends `required`. Refuses, in order: an inbound-only remote, a
/// caller with no usable delegation, and a delegation that does not lend the
/// capability.
fn delegated_client(
    state: &AppState,
    ctx: &HandlerContext,
    remote_name: &str,
    required: &str,
) -> HandlerResult<(RemoteClient, String)> {
    ctx.require_verified()?;
    let remote = load_remote(state, remote_name)?;
    // An inbound-only remote is never dialed. Refused first: no delegation can
    // be stored for it, so a missing delegation would name the wrong cure.
    remote.outbound_url()?;
    let delegator = delegator_fingerprint(ctx).map_err(|error| delegation_handler_error(&error))?;
    let client =
        RemoteClient::from_remote_cfg_as_delegated(state, &remote, ctx).map_err(client_error)?;
    let delegation = ryeos_node_runtime::remote_delegation::load_delegation(
        &ryeos_engine::roots::RuntimeRoot::new(state.config.app_root.clone()).delegations_dir(),
        &delegator,
        &remote.name,
        &state.identity,
    )
    .map_err(|error| HandlerError::Internal(format!("read the delegation: {error:#}")))?
    .ok_or_else(|| {
        delegation_handler_error(&DelegationError::Missing {
            delegator: format!("fp:{delegator}"),
            remote: remote.name.clone(),
        })
    })?;
    if !delegation.body.covers(required) {
        return Err(scope_missing(&remote.name, required));
    }
    Ok((client, delegator))
}

fn client_error(error: anyhow::Error) -> HandlerError {
    if let Some(refusal) = error.downcast_ref::<DelegationError>() {
        return delegation_handler_error(refusal);
    }
    if let Some(pin) = error.downcast_ref::<config::RemotePinError>() {
        return HandlerError::Structured {
            code: pin.code().to_owned(),
            status: 409,
            body: json!({ "code": pin.code(), "message": pin.to_string() }),
        };
    }
    HandlerError::Internal(format!("open the remote: {error:#}"))
}

/// The machine code a remote put in its error body, under either name the
/// remote's routes use.
fn remote_code(body: &Value) -> Option<&str> {
    body.get("code")
        .or_else(|| body.get("error_code"))
        .and_then(Value::as_str)
}

/// Turn the remote's answer into a refusal that names its cure: a refused
/// request keeps the remote's reason, a refused delegation or grant keeps its
/// code, and a capability the remote's grant lacks names that capability.
fn remote_error(error: anyhow::Error, remote: &str, required: &str) -> HandlerError {
    let Some(http) = error.downcast_ref::<RemoteHttpError>() else {
        return HandlerError::Internal(format!(
            "remote `{remote}` could not be reached: {error:#}"
        ));
    };
    let status = http.status.as_u16();
    let body = serde_json::from_str::<Value>(&http.body)
        .unwrap_or_else(|_| Value::String(http.body.clone()));
    match remote_code(&body) {
        Some("offline_operation_refused") => {
            let reason = body.get("reason").cloned().unwrap_or(Value::Null);
            let message = body
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the remote refused the request");
            return HandlerError::Structured {
                code: REMOTE_REFUSED.to_owned(),
                status,
                body: json!({
                    "code": REMOTE_REFUSED,
                    "error": format!("remote `{remote}` refused the request: {message}"),
                    "remote": remote,
                    "reason": reason,
                    "retryable": body.get("retryable").cloned().unwrap_or(Value::Bool(false)),
                }),
            };
        }
        Some(code) if status == 401 => {
            // The remote proved who the caller is and refused the delegation or
            // the grant; its message names the cure.
            let message = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the remote refused the delegated call");
            return HandlerError::Structured {
                code: code.to_owned(),
                status: 403,
                body: json!({
                    "code": code,
                    "error": format!("remote `{remote}`: {message}"),
                    "remote": remote,
                }),
            };
        }
        _ => {}
    }
    if status == 403 && http.body.contains(required) {
        return HandlerError::Structured {
            code: GRANT_SCOPE_MISSING.to_owned(),
            status: 403,
            body: json!({
                "code": GRANT_SCOPE_MISSING,
                "error": format!(
                    "remote `{remote}` admitted you, but its grant to you does not name \
                     `{required}`; the remote's local operator must add it to your \
                     remote_principal grant"
                ),
                "remote": remote,
                "required_scope": required,
            }),
        };
    }
    crate::remote::client::map_remote_call_error(error, format!("remote `{remote}`"))
}

pub async fn submit(
    req: SubmitRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    let (client, delegator) = delegated_client(&state, &ctx, &req.remote, REMOTE_SUBMIT_CAP)?;
    // The remote queues a request only from the key that signed it, and the
    // remote sees the caller as that key. Say so here, with the fix.
    if req.request.body.signer_fingerprint != delegator {
        return Err(HandlerError::Structured {
            code: SIGNER_NOT_CALLER.to_owned(),
            status: 403,
            body: json!({
                "code": SIGNER_NOT_CALLER,
                "error": format!(
                    "the request is signed by {}, but you are fp:{delegator}; the remote queues a \
                     request only from the key that signed it, so sign it with your own key",
                    req.request.body.signer_fingerprint
                ),
            }),
        });
    }
    let mut body = json!({ "request": req.request, "inputs": req.inputs });
    if req.stop == Some(StopWhen::Now) {
        body["stop"] = json!("now");
    }
    let reply = client
        .offline_operations_submit(&body)
        .await
        .map_err(|error| remote_error(error, &req.remote, REMOTE_SUBMIT_CAP))?;
    Ok(json!({ "remote": req.remote, "result": reply }))
}

pub async fn facts(
    req: ReadRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    let (client, _) = delegated_client(&state, &ctx, &req.remote, REMOTE_FACTS_CAP)?;
    let mut report = client
        .offline_operations_facts()
        .await
        .map_err(|error| remote_error(error, &req.remote, REMOTE_FACTS_CAP))?;
    // The facts document is what `offline-operation sign --facts` reads; the
    // remote's name rides alongside.
    report["remote"] = json!(req.remote);
    Ok(report)
}

pub async fn status(
    req: ReadRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    let (client, _) = delegated_client(&state, &ctx, &req.remote, REMOTE_STATUS_CAP)?;
    let mut report = client
        .offline_operations_status()
        .await
        .map_err(|error| remote_error(error, &req.remote, REMOTE_STATUS_CAP))?;
    report["remote"] = json!(req.remote);
    Ok(report)
}

pub const SUBMIT: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/offline-operations-submit",
    endpoint: "remote.offline-operations-submit",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/offline-operations-submit"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: SubmitRequest = crate::handler_error::parse_request(params)?;
            submit(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const FACTS: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/offline-operations-facts",
    endpoint: "remote.offline-operations-facts",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/offline-operations-facts"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ReadRequest = crate::handler_error::parse_request(params)?;
            facts(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const STATUS: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/offline-operations-status",
    endpoint: "remote.offline-operations-status",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/offline-operations-status"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ReadRequest = crate::handler_error::parse_request(params)?;
            status(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    fn http_error(status: u16, body: Value) -> anyhow::Error {
        anyhow::Error::new(RemoteHttpError {
            method: "POST",
            url: "http://hosted.example/offline-operations".to_owned(),
            status: reqwest::StatusCode::from_u16(status).unwrap(),
            body: body.to_string(),
        })
    }

    fn structured(error: HandlerError) -> (String, u16, Value) {
        match error {
            HandlerError::Structured { code, status, body } => (code, status, body),
            other => panic!("expected a structured refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_refused_request_keeps_the_remote_reason() {
        let (code, status, body) = structured(remote_error(
            http_error(
                409,
                json!({
                    "code": "offline_operation_refused",
                    "reason": "stale_ledger_head",
                    "message": "the request was not signed against this node's current ledger head",
                    "retryable": false,
                }),
            ),
            "hosted",
            REMOTE_SUBMIT_CAP,
        ));
        assert_eq!(code, REMOTE_REFUSED);
        assert_eq!(status, 409);
        assert_eq!(body["reason"], "stale_ledger_head");
        assert_eq!(body["remote"], "hosted");
        assert!(body["error"].as_str().unwrap().contains("ledger head"));
    }

    #[test]
    fn a_refused_delegation_keeps_its_code_and_cure() {
        let (code, status, body) = structured(remote_error(
            http_error(
                401,
                json!({
                    "error_code": "remote_delegation_principal_not_admitted",
                    "error": "ask this node's operator to admit it",
                }),
            ),
            "hosted",
            REMOTE_SUBMIT_CAP,
        ));
        assert_eq!(code, "remote_delegation_principal_not_admitted");
        assert_eq!(status, 403);
        assert!(body["error"].as_str().unwrap().contains("admit"));
    }

    #[test]
    fn a_grant_without_the_route_capability_names_it() {
        let (code, _, body) = structured(remote_error(
            http_error(
                403,
                json!({
                    "error": format!("service denied: required '{REMOTE_SUBMIT_CAP}'"),
                }),
            ),
            "hosted",
            REMOTE_SUBMIT_CAP,
        ));
        assert_eq!(code, GRANT_SCOPE_MISSING);
        assert_eq!(body["required_scope"], REMOTE_SUBMIT_CAP);
    }

    #[test]
    fn an_unreachable_remote_is_not_a_refusal() {
        let error = remote_error(
            anyhow::anyhow!("connection refused"),
            "hosted",
            REMOTE_SUBMIT_CAP,
        );
        assert!(matches!(error, HandlerError::Internal(_)), "{error:?}");
    }

    #[test]
    fn a_missing_scope_names_the_delegation_to_sign() {
        let (code, status, body) = structured(scope_missing("hosted", REMOTE_SUBMIT_CAP));
        assert_eq!(code, DELEGATION_SCOPE_MISSING);
        assert_eq!(status, 403);
        let message = body["error"].as_str().unwrap();
        assert!(
            message.contains("ryeos remote delegate --remote hosted"),
            "{message}"
        );
        assert!(message.contains(REMOTE_SUBMIT_CAP), "{message}");
    }

    #[test]
    fn a_submit_names_its_remote_and_takes_only_the_stop_the_route_allows() {
        let parsed: Result<SubmitRequest, _> = serde_json::from_value(json!({
            "request": {},
            "inputs": {},
        }));
        assert!(parsed.is_err(), "a submit without a remote must not parse");
        let parsed: Result<ReadRequest, _> =
            serde_json::from_value(json!({ "remote": "hosted", "extra": 1 }));
        assert!(parsed.is_err());
    }
}
