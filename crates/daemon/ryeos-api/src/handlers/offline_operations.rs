//! `offline-operations/*` — deliver signed offline-operation requests to a
//! running node, and read what the node has queued and applied.
//!
//! The handlers need a stopped node, so delivery only verifies and queues:
//! the request waits in the inbox the next startup applies. A delivery that
//! says `"stop": "now"` also performs a plain operator stop, so that startup
//! comes at once; a delivery that does not say so leaves the node running.
//! The node's supervisor, or the hosted platform, starts the next
//! incarnation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ryeos_node::offline_operation::contract::{Refusal, RefusalReason, SignedRequest};
use ryeos_node::offline_operation::deliver::{
    DeliveryError, StopForOfflineOperation, accept, facts_report, status_report,
};
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

/// How long the stop waits, so the response that reports the queued request
/// reaches the caller before the listener closes.
const STOP_DELAY: Duration = Duration::from_millis(500);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub request: SignedRequest,
    /// Input bytes by their SHA-256, base64 encoded.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    /// `"now"` stops the node once the request is queued. Absent leaves the
    /// node running.
    #[serde(default)]
    pub stop: Option<StopWhen>,
}

#[derive(serde::Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum StopWhen {
    Now,
}

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ReadRequest {}

fn refusal_error(refusal: Refusal) -> HandlerError {
    let status = match refusal.reason {
        RefusalReason::Malformed
        | RefusalReason::InvalidArguments
        | RefusalReason::InvalidInputs
        | RefusalReason::InvalidLifetime
        | RefusalReason::UnknownOperation
        | RefusalReason::InputMissing
        | RefusalReason::InputMismatch
        | RefusalReason::InputOversized
        | RefusalReason::ExpectedResultMismatch => 400,
        RefusalReason::WrongNode
        | RefusalReason::UnknownSigner
        | RefusalReason::SignerClassNotPermitted
        | RefusalReason::BadSignature
        | RefusalReason::GrantChanged
        | RefusalReason::OriginMismatch
        | RefusalReason::ScopeNotGranted
        | RefusalReason::LocalSignerOnly
        | RefusalReason::NotDelegable => 403,
        RefusalReason::Expired
        | RefusalReason::FromTheFuture
        | RefusalReason::Replayed
        | RefusalReason::StaleLedgerHead
        | RefusalReason::PreconditionFailed
        | RefusalReason::InboxFull
        | RefusalReason::HandlerFailed
        | RefusalReason::Stuck => 409,
    };
    let status = if refusal.retryable { 503 } else { status };
    HandlerError::Structured {
        code: "offline_operation_refused".to_owned(),
        status,
        body: json!({
            "code": "offline_operation_refused",
            "reason": refusal.reason.code(),
            "message": refusal.detail,
            "retryable": refusal.retryable,
        }),
    }
}

fn blocking_failure(error: impl std::fmt::Display) -> HandlerError {
    HandlerError::Internal(format!("offline-operation task failed: {error}"))
}

pub async fn submit(
    req: SubmitRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    // A stop that cannot be performed is refused before anything is queued:
    // the caller asked for both or neither.
    let stop = match req.stop {
        None => None,
        Some(StopWhen::Now) => Some(
            state
                .extensions
                .get::<StopForOfflineOperation>()
                .ok_or_else(|| {
                    HandlerError::Internal(
                        "this daemon was composed without a stop for offline operations"
                            .to_owned(),
                    )
                })?,
        ),
    };
    let mut inputs = BTreeMap::new();
    for (digest, encoded) in req.inputs {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| HandlerError::BadRequest(format!("input {digest} is not base64")))?;
        inputs.insert(digest, bytes);
    }
    let app_root = state.config.app_root.clone();
    let identity = Arc::clone(&state.identity);
    let submitted_by = ctx.fingerprint.clone();
    let request = req.request;
    let accepted = tokio::task::spawn_blocking(move || {
        accept(
            &app_root,
            &identity,
            &submitted_by,
            &request,
            &inputs,
            chrono::Utc::now(),
        )
    })
    .await
    .map_err(blocking_failure)?
    .map_err(|error| match error {
        DeliveryError::Refused(refusal) => refusal_error(refusal),
        DeliveryError::Failed(error) => HandlerError::Internal(format!("{error:#}")),
    })?;
    // A request already queued asked for its stop when it was queued; asking
    // again is the caller's to decide and costs nothing.
    let stop_reported = if stop.is_some() {
        "requested"
    } else {
        "not_requested"
    };
    if let Some(stop) = stop {
        tokio::spawn(async move {
            tokio::time::sleep(STOP_DELAY).await;
            stop.request();
        });
    }
    Ok(json!({
        "request_digest": accepted.request_digest,
        "operation": accepted.operation,
        "ledger_head": accepted.ledger_head,
        "already_queued": accepted.already_queued,
        "stop": stop_reported,
    }))
}

pub async fn facts(
    _req: ReadRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    let app_root = state.config.app_root.clone();
    let identity = Arc::clone(&state.identity);
    tokio::task::spawn_blocking(move || facts_report(&app_root, &identity))
        .await
        .map_err(blocking_failure)?
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))
}

pub async fn status(
    _req: ReadRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    let app_root = state.config.app_root.clone();
    let identity = Arc::clone(&state.identity);
    tokio::task::spawn_blocking(move || status_report(&app_root, &identity))
        .await
        .map_err(blocking_failure)?
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))
}

pub const SUBMIT: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:offline-operations/submit",
    endpoint: "offline-operations.submit",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.offline-operations/submit"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: SubmitRequest = crate::handler_error::parse_request(params)?;
            submit(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const FACTS: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:offline-operations/facts",
    endpoint: "offline-operations.facts",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.offline-operations/facts"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ReadRequest = crate::handler_error::parse_request(params)?;
            facts(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const STATUS: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:offline-operations/status",
    endpoint: "offline-operations.status",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.offline-operations/status"],
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

    fn refusal_status(reason: RefusalReason) -> u16 {
        match refusal_error(Refusal::new(reason, "detail")) {
            HandlerError::Structured { status, body, .. } => {
                assert_eq!(body["reason"], reason.code());
                status
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn refusals_map_to_the_status_their_kind_deserves() {
        assert_eq!(refusal_status(RefusalReason::BadSignature), 403);
        assert_eq!(refusal_status(RefusalReason::WrongNode), 403);
        assert_eq!(refusal_status(RefusalReason::ScopeNotGranted), 403);
        assert_eq!(refusal_status(RefusalReason::Expired), 409);
        assert_eq!(refusal_status(RefusalReason::StaleLedgerHead), 409);
        assert_eq!(refusal_status(RefusalReason::Malformed), 400);
    }

    #[test]
    fn a_read_that_failed_is_retryable() {
        let error = refusal_error(Refusal::retryable(RefusalReason::UnknownSigner, "disk"));
        match error {
            HandlerError::Structured { status, body, .. } => {
                assert_eq!(status, 503);
                assert_eq!(body["retryable"], true);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_delivery_names_only_the_stop_the_design_allows() {
        assert_eq!(
            serde_json::from_value::<StopWhen>(json!("now")).unwrap(),
            StopWhen::Now
        );
        assert!(serde_json::from_value::<StopWhen>(json!("later")).is_err());
        let read: Result<ReadRequest, _> = serde_json::from_value(json!({ "extra": 1 }));
        assert!(read.is_err());
    }
}
