//! Typed handler errors for service handlers.
//!
//! Handlers return `Result<T, HandlerError>` (or `HandlerResult<T>`)
//! instead of bare `anyhow::Result<T>` when they need to produce
//! specific HTTP status codes (404, 403, 400) instead of the default
//! 500.
//!
//! `service_invocation.rs` downcasts the returned `anyhow::Error` and
//! maps each variant to the corresponding `RouteDispatchError`.

/// Typed error for service handlers.
///
/// Handlers that need ownership-denial or input-validation semantics
/// return this instead of `bail!()`. The `service_invocation` boundary
/// maps each variant to a proper HTTP status.
#[derive(thiserror::Error, Debug, Clone)]
pub enum HandlerError {
    /// The requested resource was not found, or the caller is not
    /// authorised to know it exists. Maps to 404.
    #[error("not found")]
    NotFound,

    /// Authenticated but denied. Maps to 403.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// Malformed or invalid input. Maps to 400.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// State conflict — the request collides with existing state the caller
    /// cannot override, but is nonetheless entitled to be told about (unlike
    /// `NotFound`, which hides existence). Use this — not `NotFound` — when the
    /// caller has already proven knowledge of the resource (e.g. a deploy
    /// reconcile over the caller's own snapshot). Maps to 409.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Internal server error. Maps to 500.
    #[error("internal: {0}")]
    Internal(String),

    /// Structured error with a machine-readable JSON payload.
    /// The `code` is a stable string clients can branch on;
    /// `body` is the full response object (always an object).
    /// Maps to the HTTP status encoded in `code` semantics
    /// (typically 400 for validation errors).
    ///
    /// This variant exists so handlers can return richer error
    /// payloads (e.g. per-field validation details) without
    /// flattening everything into a plain string.
    #[error("{}", structured_text(.code, .body))]
    Structured {
        code: String,
        status: u16,
        body: serde_json::Value,
    },
}

/// The text a structured error reads as: its stable code, then the sentence the
/// body carries (the operator\'s own words; a non-operator\'s body carries a
/// fixed one), so a log line and a text check see what the client does.
fn structured_text(code: &str, body: &serde_json::Value) -> String {
    let sentence = body
        .get("error")
        .and_then(|error| match error {
            serde_json::Value::String(text) => Some(text.as_str()),
            serde_json::Value::Object(map) => map.get("message").and_then(|m| m.as_str()),
            _ => None,
        })
        .filter(|text| !text.is_empty());
    match sentence {
        Some(text) => format!("structured error: {code}: {text}"),
        None => format!("structured error: {code}"),
    }
}

/// Convenience alias for handler return types.
pub type HandlerResult<T> = Result<T, HandlerError>;

/// Strict JSON parse helper. Malformed input produces `BadRequest`
/// instead of a generic 500.
pub fn parse_request<T: serde::de::DeserializeOwned>(
    params: serde_json::Value,
) -> HandlerResult<T> {
    serde_json::from_value(params).map_err(|e| HandlerError::BadRequest(format!("{e}")))
}

impl From<serde_json::Error> for HandlerError {
    fn from(e: serde_json::Error) -> Self {
        HandlerError::BadRequest(format!("{e:#}"))
    }
}

/// Attempt to extract a `HandlerError` from an `anyhow::Error`.
///
/// Returns `Some(HandlerError)` if the error chain contains a
/// `HandlerError`, `None` otherwise. Walks the error source chain
/// to find the typed error.
pub fn extract_handler_error(err: &anyhow::Error) -> Option<HandlerError> {
    // anyhow::Error can wrap HandlerError directly since HandlerError
    // implements std::error::Error. Try downcasting the root first.
    if let Some(he) = err.downcast_ref::<HandlerError>() {
        return Some(he.clone());
    }
    // Walk the source chain.
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err.as_ref());
    while let Some(e) = source {
        if let Some(he) = e.downcast_ref::<HandlerError>() {
            return Some(he.clone());
        }
        if let Some(refusal) = typed_operator_refusal(e) {
            return Some(refusal);
        }
        source = e.source();
    }
    None
}

/// A refusal a caller caused (bad input, a missing record, a policy or a
/// state that does not admit the request) with its stable code and status. The
/// message is the sentence the operator reads; a non-operator caller receives
/// the code and status with a fixed sentence. A fault of the node itself is not
/// a refusal and stays an untyped error.
pub fn typed_refusal(code: &str, status: u16, message: impl Into<String>) -> HandlerError {
    HandlerError::Structured {
        code: code.to_owned(),
        status,
        body: serde_json::json!({
            "code": code,
            "error": message.into(),
            "retryable": false,
        }),
    }
}

/// The structured response for a refused remote delegation: the stable code,
/// the message naming the cure, and 403 because the caller is known but not
/// authorized to act this way.
pub fn delegation_handler_error(
    refusal: &ryeos_node_runtime::remote_delegation::DelegationError,
) -> HandlerError {
    HandlerError::Structured {
        code: refusal.code().to_owned(),
        status: 403,
        body: serde_json::json!({
            "code": refusal.code(),
            "error": refusal.to_string(),
        }),
    }
}

/// The structured response for a refused sealed-vault name: the stable
/// code, the message naming the cure, and 403 because the caller is known
/// but the name's ceiling does not admit it.
pub fn vault_name_handler_error(refusal: &crate::vault::VaultNameRefusal) -> HandlerError {
    HandlerError::Structured {
        code: refusal.code().to_owned(),
        status: 403,
        body: serde_json::json!({
            "code": refusal.code(),
            "error": refusal.message(),
            "name": refusal.name,
            "principal": refusal.principal,
        }),
    }
}

/// The structured response for a selection that disagrees with the remote
/// testimony: 409 because the request is well formed and conflicts with the
/// testimony the node holds; only a different selection cures it.
pub fn selected_qualification_handler_error(
    refusal: &crate::operator_external_content::qualification_testimony::SelectedQualificationRefused,
) -> HandlerError {
    HandlerError::Structured {
        code: refusal.code().to_owned(),
        status: 409,
        body: serde_json::json!({
            "code": refusal.code(),
            "error": refusal.to_string(),
            "retryable": false,
        }),
    }
}

/// The HTTP status of a qualification admission refusal: 403 when the
/// receiving policy denies it (as for the other policy refusals, the caller is
/// known but the policy does not admit the request), 409 when what it was
/// issued or accepted against has moved on and only a fresh issue or
/// acceptance cures it.
pub fn qualification_refusal_status(
    refusal: &ryeos_state::external_content::products::qualification::QualificationRefused,
) -> u16 {
    use ryeos_state::external_content::products::qualification::QualificationRefusalClass;
    match refusal.class {
        QualificationRefusalClass::PolicyDenied => 403,
        QualificationRefusalClass::Stale => 409,
    }
}

/// The structured response for a qualification admission refusal; never
/// retryable as is.
pub fn qualification_refusal_handler_error(
    refusal: &ryeos_state::external_content::products::qualification::QualificationRefused,
) -> HandlerError {
    HandlerError::Structured {
        code: refusal.code().to_owned(),
        status: qualification_refusal_status(refusal),
        body: serde_json::json!({
            "code": refusal.code(),
            "error": refusal.to_string(),
            "retryable": false,
        }),
    }
}

/// Operator-correctable refusals raised deep in app code keep their own
/// type (and their full message for non-HTTP callers); this is the single
/// place they become a structured response.
fn typed_operator_refusal(error: &(dyn std::error::Error + 'static)) -> Option<HandlerError> {
    if let Some(missing) =
        error.downcast_ref::<crate::external_content_admission::ContentBindingMissing>()
    {
        return Some(missing.handler_error());
    }
    if let Some(refused) =
        error.downcast_ref::<crate::operator_external_content::rebind::RebindRefused>()
    {
        return Some(refused.handler_error());
    }
    if let Some(refusal) =
        error.downcast_ref::<ryeos_node_runtime::remote_delegation::DelegationError>()
    {
        return Some(delegation_handler_error(refusal));
    }
    if let Some(refused) = error
        .downcast_ref::<crate::operator_external_content::qualification_testimony::SelectedQualificationRefused>()
    {
        return Some(selected_qualification_handler_error(refused));
    }
    if let Some(refused) = error.downcast_ref::<ryeos_state::external_content::products::qualification::QualificationRefused>() {
        return Some(qualification_refusal_handler_error(refused));
    }
    if let Some(engine_error) = error.downcast_ref::<ryeos_engine::error::EngineError>() {
        return project_shadow_handler_error(engine_error);
    }
    None
}

/// Stable code for a ref left unavailable because a project file shadows an
/// installed bundle item of a kind that refuses it.
pub const PROJECT_SHADOWS_BUNDLE_ITEM_CODE: &str = "project_shadows_bundle_item";

/// The structured response for a project file shadowing an installed bundle
/// item of a kind whose signed schema refuses it: 409 because the request is
/// well formed and the conflict is in the project's files, with both files
/// named so the operator can move the project one.
pub fn project_shadow_handler_error(
    error: &ryeos_engine::error::EngineError,
) -> Option<HandlerError> {
    use ryeos_engine::error::EngineError;

    match error {
        EngineError::Shared(inner) => project_shadow_handler_error(inner),
        EngineError::ProjectShadowsBundleItem {
            canonical_ref,
            kind,
            bundle,
            project_path,
            bundle_path,
        } => Some(HandlerError::Structured {
            code: PROJECT_SHADOWS_BUNDLE_ITEM_CODE.to_owned(),
            status: 409,
            body: serde_json::json!({
                "code": PROJECT_SHADOWS_BUNDLE_ITEM_CODE,
                "error": error.to_string(),
                "item_ref": canonical_ref,
                "kind": kind,
                "bundle": bundle,
                "project_path": project_path.display().to_string(),
                "bundle_path": bundle_path.display().to_string(),
                "retryable": false,
                "remediation": "rename or remove the project file",
            }),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod project_shadow_tests {
    use super::*;

    fn refusal() -> ryeos_engine::error::EngineError {
        ryeos_engine::error::EngineError::ProjectShadowsBundleItem {
            canonical_ref: "surface:shared/root".to_owned(),
            kind: "surface".to_owned(),
            bundle: "shipper".to_owned(),
            project_path: "/project/.ai/surfaces/shared/root.yaml".into(),
            bundle_path: "/bundles/shipper/.ai/surfaces/shared/root.yaml".into(),
        }
    }

    /// A handler that fails on the engine's shadow refusal answers with the
    /// stable code and a message naming the project file and the bundle,
    /// whether the refusal is the error itself, shared, or under context.
    #[test]
    fn project_shadow_refusal_becomes_a_named_conflict_with_both_files() {
        let direct = anyhow::Error::new(refusal());
        let shared = anyhow::Error::new(ryeos_engine::error::EngineError::Shared(
            std::sync::Arc::new(refusal()),
        ));
        let wrapped = anyhow::Error::new(refusal()).context("compile the project surface");
        for error in [direct, shared, wrapped] {
            match extract_handler_error(&error) {
                Some(HandlerError::Structured { code, status, body }) => {
                    assert_eq!(code, PROJECT_SHADOWS_BUNDLE_ITEM_CODE);
                    assert_eq!(status, 409);
                    assert_eq!(body["item_ref"], "surface:shared/root");
                    assert_eq!(body["bundle"], "shipper");
                    assert_eq!(
                        body["project_path"],
                        "/project/.ai/surfaces/shared/root.yaml"
                    );
                    let message = body["error"].as_str().expect("message");
                    assert!(message.contains("surface:shared/root"), "{message}");
                    assert!(message.contains("shipper"), "{message}");
                    assert!(message.contains("unavailable"), "{message}");
                }
                other => panic!("expected the named shadow conflict, got {other:?}"),
            }
        }
    }

    #[test]
    fn other_engine_errors_are_not_mapped_to_the_shadow_conflict() {
        let error = anyhow::Error::new(ryeos_engine::error::EngineError::UnsupportedKind {
            kind: "widget".to_owned(),
        });
        assert!(extract_handler_error(&error).is_none());
    }

    #[test]
    fn selected_qualification_refusal_maps_to_409_non_retryable() {
        let refusal = anyhow::Error::new(
            crate::operator_external_content::qualification_testimony::SelectedQualificationRefused::new(
                "remote product testimony differs from the selected qualification",
            ),
        )
        .context("admit selection");
        match extract_handler_error(&refusal) {
            Some(HandlerError::Structured { code, status, body }) => {
                assert_eq!(code, "selected_qualification_refused");
                assert_eq!(status, 409);
                assert_eq!(body["retryable"], false);
            }
            other => panic!("expected the typed refusal, got {other:?}"),
        }
        assert!(extract_handler_error(&anyhow::anyhow!("boom")).is_none());
    }

    #[test]
    fn qualification_refusals_map_by_class_and_are_not_retryable() {
        use ryeos_state::external_content::products::qualification::QualificationRefused;
        for (error, code, expected) in [
            (
                QualificationRefused::policy_denied("lane not allowed"),
                "qualification_policy_denied",
                403,
            ),
            (
                QualificationRefused::stale("policy changed since acceptance"),
                "qualification_stale",
                409,
            ),
        ] {
            let wrapped = error.context("admit remote testimony");
            match extract_handler_error(&wrapped) {
                Some(HandlerError::Structured {
                    code: got,
                    status,
                    body,
                }) => {
                    assert_eq!(got, code);
                    assert_eq!(status, expected);
                    assert_eq!(body["retryable"], false);
                }
                other => panic!("expected the typed refusal, got {other:?}"),
            }
        }
    }
}
