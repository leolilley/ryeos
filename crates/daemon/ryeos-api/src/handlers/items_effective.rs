//! `items.effective` — resolve, verify, compose, and return an effective item.
//!
//! Works for any item kind (executable or not). The engine owns the
//! resolution/trust/composition semantics; this handler is intentionally
//! only a typed service wrapper.
//!
//! ## Error codes
//!
//! The handler maps engine errors to `HandlerError` variants whose messages
//! carry a stable prefix that clients can branch on:
//!
//! | Prefix | HTTP | Meaning |
//! |--------|------|---------|
//! | `invalid canonical ref` | 400 | Malformed canonical ref string |
//! | `wrong_kind:` | 400 | `expected_kind` guard failed |
//! | `untrusted:` | 403 | Signer not in trust store |
//! | `composition_failed:` | 400 | Composer chain error |
//! | `parse_failed:` | 400 | Parser produced invalid output |
//! | `contract_violation:` | 400 | Structured envelope — see below |
//! | (none) | 404 | Item not found in any space |
//! | (none) | 500 | Unexpected internal error |
//!
//! ## Contract violation envelope
//!
//! When the composed value fails validation against the kind's contract,
//! the response is a structured JSON object:
//!
//! ```text
//! {
//!   "error": "<summary message>",
//!   "error_code": "contract_violation",
//!   "details": {
//!     "errors": [
//!       { "path": "<field>", "code": "<code>", "expected": "<...>", "found": "<...>" },
//!       ...
//!     ],
//!     "warnings": [
//!       { "path": "<field>", "code": "<code>", "expected": "<...>", "found": "<...>" },
//!       ...
//!     ]
//!   }
//! }
//! ```

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::EffectiveItemRequest;
use ryeos_engine::error::EngineError;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Canonical item ref to resolve, e.g. "surface:ryeos/cockpit/base".
    pub canonical_ref: String,

    /// Optional project path for project-space resolution.
    /// When absent, only system and user spaces are searched.
    #[serde(default)]
    pub project_path: Option<String>,

    /// Optional guardrail for clients that know which kind they expect.
    #[serde(default)]
    pub expected_kind: Option<String>,
}

pub async fn handle(req: Request, _ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let item_ref = CanonicalRef::parse(&req.canonical_ref).map_err(|e| {
        HandlerError::BadRequest(format!(
            "invalid canonical ref '{}': {e}",
            req.canonical_ref
        ))
    })?;

    let effective = state
        .engine
        .effective_item(EffectiveItemRequest {
            item_ref,
            expected_kind: req.expected_kind,
            project_root: req.project_path.map(std::path::PathBuf::from),
        })
        .map_err(map_engine_error)?;

    serde_json::to_value(effective).map_err(Into::into)
}

/// Map typed engine errors to HTTP-appropriate handler errors with
/// stable error codes. Renderers can branch on these instead of
/// parsing error messages.
fn map_engine_error(e: EngineError) -> HandlerError {
    match &e {
        EngineError::EffectiveItemNotFound { canonical_ref: _ } => HandlerError::NotFound,
        EngineError::EffectiveItemWrongKind {
            expected,
            found,
            canonical_ref,
        } => HandlerError::BadRequest(format!(
            "wrong_kind: expected `{expected}`, got `{found}` for `{canonical_ref}`"
        )),
        EngineError::EffectiveItemUntrusted {
            canonical_ref,
            fingerprint,
        } => HandlerError::Forbidden(format!(
            "untrusted: `{canonical_ref}` (fingerprint: {fingerprint})"
        )),
        EngineError::EffectiveItemCompositionFailed { reason, .. } => {
            HandlerError::BadRequest(format!("composition_failed: {reason}"))
        }
        EngineError::EffectiveItemParseFailed { reason, .. } => {
            HandlerError::BadRequest(format!("parse_failed: {reason}"))
        }
        EngineError::ComposedValueContractViolation {
            canonical_ref,
            report,
        } => {
            let error_count = report.errors.len();
            let warn_count = report.warnings.len();
            let error_suffix = if error_count == 1 { "" } else { "s" };
            let warning_suffix = if warn_count == 1 { "" } else { "s" };
            let summary = format!(
                "contract_violation: `{canonical_ref}` ({error_count} error{error_suffix}, {warn_count} warning{warning_suffix})",
            );

            let violations_to_json =
                |violations: &[ryeos_engine::contracts::InstanceViolation]| {
                    violations
                        .iter()
                        .map(|v| {
                            serde_json::json!({
                                "path": v.path,
                                "code": v.code.to_string(),
                                "expected": v.expected,
                                "found": v.found,
                            })
                        })
                        .collect::<Vec<_>>()
                };

            let body = serde_json::json!({
                "error": summary,
                "error_code": "contract_violation",
                "details": {
                    "errors": violations_to_json(&report.errors),
                    "warnings": violations_to_json(&report.warnings),
                }
            });

            HandlerError::Structured {
                code: "contract_violation".into(),
                body,
            }
        }
        _ => HandlerError::Internal(e.to_string()),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:items/effective",
    endpoint: "items.effective",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use ryeos_engine::contracts::{
        InstanceValidationReport, InstanceViolation, InstanceViolationCode,
    };
    use ryeos_engine::error::EngineError;

    use super::map_engine_error;

    fn violation(
        path: &str,
        code: InstanceViolationCode,
        expected: &str,
        found: &str,
    ) -> InstanceViolation {
        InstanceViolation {
            path: path.to_string(),
            code,
            expected: expected.to_string(),
            found: found.to_string(),
        }
    }

    #[test]
    fn contract_violation_maps_to_structured_error() {
        let report = InstanceValidationReport {
            errors: vec![
                violation(
                    "launch.mode",
                    InstanceViolationCode::EnumMismatch,
                    "one of [\"managed\", \"edge\"]",
                    "\"local\"",
                ),
                violation(
                    "name",
                    InstanceViolationCode::TypeMismatch,
                    "string",
                    "null",
                ),
            ],
            warnings: vec![violation(
                "affordances",
                InstanceViolationCode::UnexpectedField,
                "no field",
                "present",
            )],
        };

        let err = EngineError::ComposedValueContractViolation {
            canonical_ref: "surface:ryeos/cockpit/base".into(),
            report,
        };

        let he = map_engine_error(err);

        match he {
            crate::handler_error::HandlerError::Structured { code, body } => {
                assert_eq!(code, "contract_violation");
                assert_eq!(body["error_code"], "contract_violation");
                assert!(body["error"]
                    .as_str()
                    .unwrap()
                    .contains("surface:ryeos/cockpit/base"));
                assert!(body["error"].as_str().unwrap().contains("2 errors"));
                assert!(body["error"].as_str().unwrap().contains("1 warning"));

                let errors = body["details"]["errors"].as_array().unwrap();
                assert_eq!(errors.len(), 2);
                assert_eq!(errors[0]["path"], "launch.mode");
                assert_eq!(errors[0]["code"], "enum_mismatch");
                assert_eq!(errors[0]["expected"], "one of [\"managed\", \"edge\"]");
                assert_eq!(errors[0]["found"], "\"local\"");
                assert_eq!(errors[1]["path"], "name");
                assert_eq!(errors[1]["code"], "type_mismatch");

                let warnings = body["details"]["warnings"].as_array().unwrap();
                assert_eq!(warnings.len(), 1);
                assert_eq!(warnings[0]["path"], "affordances");
                assert_eq!(warnings[0]["code"], "unexpected_field");
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    #[test]
    fn contract_violation_with_single_error_uses_singular() {
        let report = InstanceValidationReport {
            errors: vec![violation(
                "name",
                InstanceViolationCode::TypeMismatch,
                "string",
                "null",
            )],
            warnings: vec![],
        };

        let err = EngineError::ComposedValueContractViolation {
            canonical_ref: "tool:my/tool".into(),
            report,
        };

        let he = map_engine_error(err);
        match he {
            crate::handler_error::HandlerError::Structured { body, .. } => {
                let msg = body["error"].as_str().unwrap();
                // Singular: "1 error", not "1 errors"
                assert!(
                    msg.contains("1 error"),
                    "message should use singular: {msg}"
                );
                assert!(
                    msg.contains("0 warnings"),
                    "message should use plural for 0: {msg}"
                );
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    #[test]
    fn contract_violation_empty_report() {
        let report = InstanceValidationReport {
            errors: vec![],
            warnings: vec![],
        };

        let err = EngineError::ComposedValueContractViolation {
            canonical_ref: "directive:test/x".into(),
            report,
        };

        let he = map_engine_error(err);
        match he {
            crate::handler_error::HandlerError::Structured { body, .. } => {
                let errors = body["details"]["errors"].as_array().unwrap();
                let warnings = body["details"]["warnings"].as_array().unwrap();
                assert!(errors.is_empty());
                assert!(warnings.is_empty());
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }
}
