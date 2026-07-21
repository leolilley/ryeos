//! Wire-shape struct shared by all structured daemon error surfaces:
//! `/execute` JSON body, `/execute/stream` SSE `stream_error` event,
//! and `thread_failed.payload.error`.
//!
//! Keeping a single struct guarantees the three surfaces stay in sync.
//! Adding a new field is a one-line change here, not a three-site
//! search-and-replace.
//!
//! Construction is via `From<&DispatchError>` for the cases that map
//! cleanly. For paths that don't have a `DispatchError` in hand
//! (resume preflight in `runner.rs` only sees `MaterializationError`),
//! call `StructuredErrorPayload::required_secret_missing(...)` or
//! whichever generic constructor matches.

use serde::Serialize;

/// Stable wire shape for structured daemon errors. Every field except
/// `code` and `error` is optional so unrelated error variants can
/// serialize the same struct without writing nulls.
#[derive(Debug, Clone, Serialize)]
pub struct StructuredErrorPayload {
    /// Stable machine-readable error code (e.g. `"required_secret_missing"`).
    pub code: String,
    /// Human-readable error message. Always present.
    pub error: String,
    /// Whether retrying the same request may succeed without an authored or
    /// configuration change.
    pub retryable: bool,
    /// Required-secret-missing fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    /// Cap-denial field (existing `MissingCap` surface).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_cap: Option<String>,
    /// Generic launch-preparation fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// Contract-violation details: per-field errors and warnings.
    /// Matches the `items.effective` `contract_violation` envelope shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    /// Service-not-installed fields: the requested item ref, the
    /// bundles registered on the answering node, and an operator hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_bundles: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl StructuredErrorPayload {
    /// Build a generic envelope with just `code` + `error`. Use for
    /// `DispatchError` variants that have no extra structured fields.
    pub fn generic(code: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            error: error.into(),
            retryable: false,
            env_var: None,
            source_kind: None,
            source_name: None,
            remediation: None,
            required_cap: None,
            classification: None,
            binding: None,
            details: None,
            item_ref: None,
            installed_bundles: None,
            hint: None,
        }
    }

    /// Build the `service_not_installed` envelope: the requested
    /// service item resolved to nothing because the bundle that ships
    /// it is not installed on the answering node.
    pub fn service_not_installed(
        error_msg: impl Into<String>,
        item_ref: impl Into<String>,
        installed_bundles: Vec<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            item_ref: Some(item_ref.into()),
            installed_bundles: Some(installed_bundles),
            hint: Some(hint.into()),
            ..Self::generic("service_not_installed", error_msg)
        }
    }

    /// Build the `required_secret_missing` envelope. Used by both
    /// the dispatch boundary (`From<&DispatchError>`) and the resume
    /// preflight path in `runner.rs` (which only has the launch-layer
    /// `MaterializationError` in hand).
    pub fn required_secret_missing(
        error_msg: impl Into<String>,
        env_var: impl Into<String>,
        source_kind: impl Into<String>,
        source_name: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            env_var: Some(env_var.into()),
            source_kind: Some(source_kind.into()),
            source_name: Some(source_name.into()),
            remediation: Some(remediation.into()),
            ..Self::generic("required_secret_missing", error_msg)
        }
    }

    /// Build the `missing_cap` envelope (existing `/execute` surface).
    pub fn missing_cap(error_msg: impl Into<String>, required: impl Into<String>) -> Self {
        Self {
            required_cap: Some(required.into()),
            ..Self::generic("missing_cap", error_msg)
        }
    }

    /// Build the `contract_violation` envelope. Matches the shape used by
    /// `items.effective` for `ComposedValueContractViolation` errors.
    pub fn contract_violation(
        error_msg: impl Into<String>,
        details: crate::dispatch_error::ContractViolationDetails,
    ) -> Self {
        Self {
            details: Some(
                serde_json::to_value(details).expect("ContractViolationDetails always serializes"),
            ),
            ..Self::generic("contract_violation", error_msg)
        }
    }

    /// Convert to `serde_json::Value` for surfaces that need a `Value`
    /// (the SSE helper, terminal event payload).
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("StructuredErrorPayload always serializes")
    }
}

impl From<&crate::dispatch_error::DispatchError> for StructuredErrorPayload {
    fn from(e: &crate::dispatch_error::DispatchError) -> Self {
        use crate::dispatch_error::DispatchError;
        let mut payload = match e {
            DispatchError::RequiredSecretMissing {
                env_var,
                source_kind,
                source_name,
                remediation,
                ..
            } => Self::required_secret_missing(
                e.to_string(),
                env_var,
                source_kind,
                source_name,
                remediation,
            ),
            DispatchError::ExecutionNotRestartEligible {
                item_ref,
                remediation,
                ..
            } => Self {
                item_ref: Some(item_ref.clone()),
                remediation: Some(remediation.clone()),
                ..Self::generic(e.code(), e.to_string())
            },
            DispatchError::MissingCap { required } => Self::missing_cap(e.to_string(), required),
            DispatchError::ServiceNotInstalled {
                service_ref,
                installed_bundles,
                ..
            } => Self::service_not_installed(
                e.to_string(),
                service_ref,
                installed_bundles.clone(),
                "install the bundle that provides this service item on the target node, \
                 or deploy an image profile that includes it",
            ),
            DispatchError::ComposedValueContractViolation { details, .. } => {
                Self::contract_violation(e.to_string(), details.clone())
            }
            DispatchError::LaunchPreparationFailed {
                code,
                classification,
                binding,
                details,
                ..
            } => Self {
                classification: Some(classification.clone()),
                binding: binding.clone(),
                details: (!details.is_empty()).then(|| {
                    serde_json::to_value(details)
                        .expect("launch diagnostic scalar map always serializes")
                }),
                ..Self::generic(code, e.to_string())
            },
            DispatchError::LaunchPolicyForbidden { code, binding, .. }
            | DispatchError::LaunchResourceNotFound { code, binding, .. } => Self {
                binding: binding.clone(),
                ..Self::generic(code, e.to_string())
            },
            _ => Self::generic(e.code(), e.to_string()),
        };
        payload.retryable = e.retryable();
        payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_payload_has_no_details() {
        let p = StructuredErrorPayload::generic("test_code", "test error");
        let v = p.to_value();
        assert_eq!(v["code"], "test_code");
        assert_eq!(v["error"], "test error");
        assert!(
            v.get("details").is_none(),
            "generic must not include details"
        );
    }

    #[test]
    fn contract_violation_payload_has_details() {
        let details = crate::dispatch_error::ContractViolationDetails {
            errors: vec![crate::dispatch_error::ContractViolationEntry {
                path: "launch.mode".to_string(),
                code: "enum_mismatch".to_string(),
                expected: "\"wait\"".to_string(),
                found: "\"bogus\"".to_string(),
            }],
            warnings: vec![],
        };
        let p = StructuredErrorPayload::contract_violation(
            "contract violation: `work:x` (1 error, 0 warnings)",
            details,
        );
        let v = p.to_value();
        assert_eq!(v["code"], "contract_violation");
        let det = &v["details"];
        assert_eq!(det["errors"].as_array().unwrap().len(), 1);
        assert_eq!(det["errors"][0]["path"], "launch.mode");
        assert_eq!(det["errors"][0]["code"], "enum_mismatch");
        assert_eq!(det["warnings"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn from_dispatch_error_contract_violation() {
        let details = crate::dispatch_error::ContractViolationDetails {
            errors: vec![crate::dispatch_error::ContractViolationEntry {
                path: "launch.mode".to_string(),
                code: "missing_required_field".to_string(),
                expected: "a string".to_string(),
                found: "<missing>".to_string(),
            }],
            warnings: vec![crate::dispatch_error::ContractViolationEntry {
                path: "extra".to_string(),
                code: "unexpected_field".to_string(),
                expected: "<none>".to_string(),
                found: "present".to_string(),
            }],
        };
        let de = crate::dispatch_error::DispatchError::ComposedValueContractViolation {
            canonical_ref: "work:foo/bar".to_string(),
            error_count: 1,
            warning_count: 1,
            details,
        };
        let p = StructuredErrorPayload::from(&de);
        assert_eq!(p.code, "contract_violation");
        let v = p.to_value();
        assert!(v.get("details").is_some());
        assert_eq!(v["details"]["errors"].as_array().unwrap().len(), 1);
        assert_eq!(v["details"]["warnings"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn from_dispatch_error_generic_has_no_details() {
        let de =
            crate::dispatch_error::DispatchError::InvalidRef("x".to_string(), "bad".to_string());
        let p = StructuredErrorPayload::from(&de);
        assert_eq!(p.code, "invalid_ref");
        assert!(p.details.is_none());
    }

    #[test]
    fn restart_ineligible_payload_carries_remediation_without_retry() {
        let error = crate::dispatch_error::DispatchError::ExecutionNotRestartEligible {
            item_ref: "tool:test/node-policy".to_string(),
            reason: "node policy is mutable".to_string(),
            remediation: "use verified content".to_string(),
        };
        let value = StructuredErrorPayload::from(&error).to_value();
        assert_eq!(value["code"], "execution_not_restart_eligible");
        assert_eq!(value["item_ref"], "tool:test/node-policy");
        assert_eq!(value["remediation"], "use verified content");
        assert_eq!(value["retryable"], false);
    }
}
