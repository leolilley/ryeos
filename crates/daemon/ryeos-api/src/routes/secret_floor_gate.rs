//! The secret floor where a route serves records or answers outside the
//! service executor.
//!
//! A value the node holds in its secret store must not reach a caller that is
//! not the node's operator. Service answers pass the floor in the executor;
//! the paths here (event streams, the buffered execute answer, the handler
//! envelope, the seat streams) do not pass through it, so each builds one
//! [`CallerFloor`] for its request and applies it to whatever it serves.
//!
//! The decision is the caller's class, not the kind of client: an admitted
//! operator gets `None` and its records are never touched; every other caller,
//! verified or not, gets the floor over every secret value the node holds. A
//! floor that cannot be built is a refusal, never a pass-through.

use std::sync::Arc;

use ryeos_app::secret_floor::{SecretFloor, withheld_refusal};
use ryeos_app::state::AppState;
use ryeos_app::stream_envelope::RouteStreamEnvelope;
use serde_json::Value;

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::RoutePrincipal;

/// The floor one request applies, shared by every record it serves. `None` is
/// the operator: nothing is masked or refused.
pub type CallerFloor = Option<Arc<SecretFloor>>;

/// The typed refusal of an answer the floor withheld, as a route error.
pub fn withheld() -> RouteDispatchError {
    match withheld_refusal() {
        ryeos_app::handler_error::HandlerError::Structured { code, status, body } => {
            RouteDispatchError::Structured {
                code,
                status,
                body,
                thread_id: None,
            }
        }
        other => RouteDispatchError::Internal(format!("{other}")),
    }
}

/// The floor for the caller `principal` names, built once for a request. A
/// request with no principal is the public. The operator is decided by the
/// same class every service answer uses.
pub fn caller_floor(
    state: &AppState,
    principal: Option<&RoutePrincipal>,
) -> Result<CallerFloor, RouteDispatchError> {
    let context = principal
        .map(RoutePrincipal::handler_context)
        .unwrap_or_else(ryeos_app::handler_context::HandlerContext::anonymous);
    let operator =
        ryeos_app::operator_authority::disclosure_class_for(state, &context).is_operator();
    floor_for(operator, || SecretFloor::for_node(state))
}

/// The decision itself: no floor for the operator, otherwise the floor
/// `build` returns, or the withheld refusal when it cannot be built.
fn floor_for(
    operator: bool,
    build: impl FnOnce() -> anyhow::Result<SecretFloor>,
) -> Result<CallerFloor, RouteDispatchError> {
    if operator {
        return Ok(None);
    }
    match build() {
        Ok(floor) => Ok(Some(Arc::new(floor))),
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "secret floor unavailable; a record is withheld from a non-operator caller"
            );
            Err(withheld())
        }
    }
}

/// An answer for the caller: the value itself when the caller is the operator
/// or it holds no secret, otherwise the withheld refusal.
pub fn withhold_answer(
    floor: Option<&SecretFloor>,
    value: Value,
) -> Result<Value, RouteDispatchError> {
    match floor {
        None => Ok(value),
        Some(floor) => floor.withhold(value).map_err(|_| withheld()),
    }
}

/// A failure for the caller: the failure itself when the caller is the
/// operator or its text and body hold no secret, otherwise the withheld
/// refusal.
pub fn withhold_error(
    floor: Option<&SecretFloor>,
    error: RouteDispatchError,
) -> RouteDispatchError {
    let Some(floor) = floor else {
        return error;
    };
    let mut carried = vec![Value::String(error.to_string())];
    if let RouteDispatchError::Structured { body, .. } = &error {
        carried.push(body.clone());
    }
    if carried.iter().any(|value| floor.carries_secret(value)) {
        withheld()
    } else {
        error
    }
}

/// One stream record for the caller: every secret in its payload is replaced
/// by the fixed marker. The whole record is matched, never a chunk of it.
pub fn mask_envelope(
    floor: Option<&SecretFloor>,
    mut envelope: RouteStreamEnvelope,
) -> RouteStreamEnvelope {
    if let Some(floor) = floor {
        floor.mask(&mut envelope.payload);
    }
    envelope
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::workload_client::SECRET_FLOOR_MARKER;
    use serde_json::json;

    const SECRET: &str = "sk-live-0123456789";

    fn floor() -> SecretFloor {
        SecretFloor::from_values(vec![SECRET.to_owned()])
    }

    #[test]
    fn the_operator_gets_no_floor_and_is_never_asked_to_build_one() {
        let built = floor_for(true, || panic!("the operator's floor is never built")).unwrap();
        assert!(built.is_none());
    }

    #[test]
    fn a_caller_that_is_not_the_operator_gets_the_floor() {
        let built = floor_for(false, || Ok(floor())).unwrap();
        assert!(built.is_some());
    }

    #[test]
    fn a_floor_that_cannot_be_built_refuses_the_caller() {
        // The floor holds secret values and is never Debug, so the refusal
        // is matched out rather than unwrapped.
        let Err(refused) = floor_for(false, || anyhow::bail!("store unreadable")) else {
            panic!("a floor that cannot be built must refuse the caller");
        };
        match refused {
            RouteDispatchError::Structured {
                code, status, body, ..
            } => {
                assert_eq!((code.as_str(), status), ("secret_floor_withheld", 403));
                assert!(!body.to_string().contains("store unreadable"));
            }
            other => panic!("not a typed refusal: {other:?}"),
        }
    }

    #[test]
    fn an_answer_with_a_secret_is_refused_to_a_non_operator_and_whole_to_the_operator() {
        let answer = json!({"result": format!("key {SECRET}")});
        let floor = floor();
        assert!(matches!(
            withhold_answer(Some(&floor), answer.clone()),
            Err(RouteDispatchError::Structured { code, .. }) if code == "secret_floor_withheld"
        ));
        assert_eq!(withhold_answer(None, answer.clone()).unwrap(), answer);
        let clean = json!({"result": "fine"});
        assert_eq!(withhold_answer(Some(&floor), clean.clone()).unwrap(), clean);
    }

    #[test]
    fn a_failure_naming_a_secret_is_replaced_for_a_non_operator_only() {
        let floor = floor();
        let leaky = || RouteDispatchError::Internal(format!("dispatch failed: {SECRET}"));
        assert!(matches!(
            withhold_error(Some(&floor), leaky()),
            RouteDispatchError::Structured { code, .. } if code == "secret_floor_withheld"
        ));
        assert!(matches!(
            withhold_error(None, leaky()),
            RouteDispatchError::Internal(text) if text.contains(SECRET)
        ));
        assert!(matches!(
            withhold_error(Some(&floor), RouteDispatchError::NotFound),
            RouteDispatchError::NotFound
        ));
    }

    #[test]
    fn a_record_is_masked_for_a_non_operator_and_unchanged_for_the_operator() {
        let record = RouteStreamEnvelope::with_id(
            "3",
            "cognition_out",
            json!({"delta": format!("token {SECRET}")}),
        );
        let floor = floor();
        let masked = mask_envelope(Some(&floor), record.clone());
        assert_eq!(masked.id.as_deref(), Some("3"));
        assert_eq!(
            masked.payload["delta"],
            format!("token {SECRET_FLOOR_MARKER}")
        );
        assert!(!masked.payload.to_string().contains(SECRET));
        assert_eq!(mask_envelope(None, record.clone()), record);
    }
}
