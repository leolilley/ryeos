//! Typed dispatch errors with explicit HTTP status mapping.
//!
//! Each enumerated variant carries the structured fields callers need
//! to reason about the failure, plus a `http_status()` method that
//! the execute response mode consults exactly once per request. Status
//! mapping is by variant, never by matching substrings of an error
//! message.
//!
//! The variant names — and the `http_status()` arms — are the source
//! of truth for `/execute` non-200 surfaces. The pin tests in
//! `crates/bin/daemon/tests/dispatch_pin.rs` lock the resulting status codes and
//! JSON shapes; if a future variant changes the status mapping, the
//! pin test catches it before the HTTP contract drifts.
//!
//! Operator-fixable failures are distinct variants with honest status
//! codes: cap denial → 403, manifest miss → 502, push-first → 409,
//! unknown service handler → 502, materialization error → 502. Only
//! truly unexpected internal errors are 500.

use std::collections::BTreeMap;

use axum::http::StatusCode;
use ryeos_handler_protocol::LaunchDiagnosticScalarWire;

/// Per-field violation details carried by
/// `DispatchError::ComposedValueContractViolation`. Structured so the
/// wire envelope can include individual violation entries matching the
/// `items.effective` `contract_violation` shape.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContractViolationDetails {
    pub errors: Vec<ContractViolationEntry>,
    pub warnings: Vec<ContractViolationEntry>,
}

/// A single field-level violation within a contract-violation report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContractViolationEntry {
    pub path: String,
    pub code: String,
    pub expected: String,
    pub found: String,
}

impl ContractViolationDetails {
    /// Build from a `ryeos_engine::contracts::InstanceValidationReport`.
    pub fn from_report(report: &ryeos_engine::contracts::InstanceValidationReport) -> Self {
        let to_entry = |v: &ryeos_engine::contracts::InstanceViolation| ContractViolationEntry {
            path: v.path.clone(),
            code: v.code.to_string(),
            expected: v.expected.clone(),
            found: v.found.clone(),
        };
        Self {
            errors: report.errors.iter().map(to_entry).collect(),
            warnings: report.warnings.iter().map(to_entry).collect(),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum DispatchError {
    #[error("invalid item ref '{0}': {1}")]
    InvalidRef(String, String),
    #[error("kind '{kind}' is not root-executable: {detail}")]
    NotRootExecutable { kind: String, detail: String },
    #[error("insufficient capabilities for runtime '{runtime}': required {required:?}, caller_scopes {caller_scopes:?}")]
    InsufficientCaps {
        runtime: String,
        required: Vec<String>,
        caller_scopes: Vec<String>,
    },
    #[error("alias cycle detected resolving '{root_ref}': visited {visited:?}")]
    AliasCycle {
        root_ref: String,
        visited: Vec<String>,
    },
    #[error("alias chain exceeded MAX_HOPS ({max_hops}) resolving '{root_ref}'")]
    AliasChainTooLong { root_ref: String, max_hops: usize },
    #[error("schema misconfigured for kind '{kind}': {detail}")]
    SchemaMisconfigured { kind: String, detail: String },
    /// `Display` is the bare reason — pin tests assert byte-equality
    /// of the wording (`"detached mode not yet supported for native runtimes"`,
    /// etc.). The variant name carries the diagnostic context.
    #[error("{reason}")]
    CapabilityRejected { reason: String },
    #[error("streaming dispatch outcome is not implemented")]
    StreamingNotImplemented,
    #[error("project source error: {0}")]
    ProjectSource(String),
    // ── operator-fixable failures (not 500) ───────────────────────
    /// Service handler not found in the in-process handler registry.
    /// The kind schema declared `InProcessHandler { Services }` but
    /// no handler matched the item's service name.
    #[error(
        "service handler not found for '{service_ref}' in {registry}; available: [{available}]"
    )]
    ServiceHandlerMissing {
        service_ref: String,
        registry: String,
        available: String,
    },
    /// Service capability denied — the caller lacks the scope required
    /// by the service handler's declared capabilities.
    #[error("service '{service_ref}' denied: caller scopes {caller_scopes:?} do not include required '{required}'")]
    ServiceCapDenied {
        service_ref: String,
        required: String,
        caller_scopes: Vec<String>,
    },
    /// Service is unavailable in the current execution mode (e.g.
    /// daemon-only service called from offline/standalone mode).
    #[error("service '{service_ref}' is unavailable in {mode} mode (requires {requires})")]
    ServiceUnavailable {
        service_ref: String,
        mode: String,
        requires: String,
    },
    /// The service item itself was not found in any installed bundle,
    /// project, or user space. Distinct from `ServiceHandlerMissing`
    /// (item YAML resolved but no compiled handler matched the
    /// endpoint): this means the bundle that ships the service item is
    /// not installed on this node. Carries the installed-bundle list so
    /// a remote operator can fix the deployment without source-level
    /// debugging.
    #[error(
        "service item '{service_ref}' was not found in installed bundles (installed: [{}])",
        installed_bundles.join(", ")
    )]
    ServiceNotInstalled {
        service_ref: String,
        installed_bundles: Vec<String>,
        searched_spaces: Vec<String>,
    },
    /// Subprocess executor missing — the resolved item's executor_ref
    /// does not correspond to a known executor.
    #[error("subprocess executor missing for '{item_ref}': {detail}")]
    SubprocessExecutorMissing { item_ref: String, detail: String },
    /// Root item has no executor_id. Terminal executors use
    /// `executor_id: null` to end a chain and are not launchable as
    /// root tools.
    #[error("root executor missing for '{item_ref}': {detail}")]
    RootExecutorMissing { item_ref: String, detail: String },
    /// Subprocess run failed — the waited or detached run encountered
    /// an error after resolution succeeded.
    #[error("subprocess run failed for '{item_ref}': {detail}")]
    SubprocessRunFailed { item_ref: String, detail: String },
    /// A daemon-owned restartable launch resolved only mutable node-policy
    /// executable authority. Refuse before thread birth rather than silently
    /// weakening its lifecycle contract.
    #[error("execution '{item_ref}' is not restart eligible: {reason}")]
    ExecutionNotRestartEligible {
        item_ref: String,
        reason: String,
        remediation: String,
    },
    /// Runtime binary materialization failed — the native executor
    /// could not be resolved from the bundle CAS.
    #[error("runtime materialization failed for '{executor_ref}': {detail}")]
    RuntimeMaterializationFailed {
        executor_ref: String,
        detail: String,
    },
    /// A pre-minted launch reservation was cancelled before thread creation or
    /// the irreversible spawn handoff.
    #[error("launch was cancelled before {stage}")]
    LaunchCancelled { stage: &'static str },
    /// A declared required secret was not found in any source.
    /// Generic at the dispatch layer; the `source_kind`/`source_name`
    /// fields attribute which subsystem demanded the secret without
    /// coupling this generic error to any executable kind. The
    /// secret resolves from sealed vault, daemon host env, or `.env`
    /// overlay; `remediation` carries the actionable string.
    #[error("required secret missing for '{item_ref}': `{env_var}` was not found in sealed vault, daemon host environment, or `.env` overlay (source: {source_kind}/{source_name})")]
    RequiredSecretMissing {
        item_ref: String,
        env_var: String,
        source_kind: String,
        source_name: String,
        remediation: String,
    },
    /// Runtime launch-contract validation or its trusted preparer rejected the
    /// request before spawn. The code/classification are protocol-owned and
    /// remain generic at this layer.
    #[error("launch preparation failed ({code}/{classification}): {message}")]
    LaunchPreparationFailed {
        code: String,
        message: String,
        classification: String,
        binding: Option<String>,
        details: Box<BTreeMap<String, LaunchDiagnosticScalarWire>>,
    },
    #[error("launch policy rejected request ({code}): {message}")]
    LaunchPolicyForbidden {
        code: String,
        message: String,
        binding: Option<String>,
    },
    #[error("launch resource was not found ({code}): {message}")]
    LaunchResourceNotFound {
        code: String,
        message: String,
        binding: Option<String>,
    },
    #[error("ref bindings are not applicable to the resolved {class} dispatch path")]
    RefBindingNotApplicable { class: String },
    /// Project source push-first — the project has not been pushed to
    /// the daemon's CAS before execution was requested. The Display
    /// is the bare wording (e.g. `"no pushed HEAD for project '<path>' \
    /// — push first"`) so the pin in `dispatch_pin.rs::\
    /// pin_native_runtime_with_pushed_head` holds byte-identically. The
    /// HTTP layer maps this variant to 409.
    #[error("{0}")]
    ProjectSourcePushFirst(String),
    /// Project source checkout failed — the pushed HEAD snapshot
    /// could not be checked out from CAS.
    #[error("project source checkout failed: {0}")]
    ProjectSourceCheckoutFailed(String),
    // ── Method dispatch errors ─────────────────────────────────────
    /// The requested method is not declared on the kind's schema.
    #[error("unknown method '{requested}' for kind '{kind}'; declared methods: [{declared}]")]
    UnknownMethod {
        kind: String,
        requested: String,
        declared: String,
    },
    /// A required arg for the method is missing or has wrong type.
    #[error("invalid arg for method '{method}': {reason}")]
    MethodInvalidArg { method: String, reason: String },
    /// The method's runtime returned a structured failure.
    #[error("method '{method}' on kind '{kind}' failed: {reason}")]
    MethodFailed {
        kind: String,
        method: String,
        reason: String,
    },
    /// The method returned NotImplemented (phase gate).
    #[error("method '{method}' on kind '{kind}' is not implemented (phase {phase})")]
    MethodNotImplemented {
        kind: String,
        method: String,
        phase: u8,
    },
    /// Projection invariant violated during slim-payload construction.
    #[error("projection invariant violated: {reason}")]
    ProjectionInvariant { reason: String },
    /// Protocol descriptor not found in the protocol registry.
    #[error("protocol `{0}` not registered")]
    ProtocolNotRegistered(String),
    /// Streaming protocol cannot be invoked with launch_mode=detached.
    #[error("streaming protocols cannot be invoked with launch_mode=detached")]
    StreamingNotDetachable,
    /// Invalid launch_mode value.
    #[error("invalid launch_mode: {other}")]
    InvalidLaunchMode { other: String },
    /// Caller lacks a required capability for the dispatch role.
    /// Mapped to 403 by the HTTP layer with body `{ "required_cap": "..." }`.
    #[error("missing required capability: {required}")]
    MissingCap { required: String },
    /// The requested resource was not found, or the caller is not
    /// authorised to know it exists. Maps to 404.
    #[error("not found")]
    NotFound,
    /// State conflict — the request collides with existing state the caller
    /// cannot override but is entitled to be told about (e.g. a deploy
    /// reconcile touching a schedule owned by another principal). The
    /// `Display` is the bare actionable message. Maps to 409.
    #[error("{0}")]
    Conflict(String),
    // ── Target-site forwarding errors ────────────────────────────
    /// The requested target site is not configured as a remote.
    #[error("unknown target site '{target_site_id}'; configured sites: [{known_sites}]")]
    UnknownTargetSite {
        target_site_id: String,
        known_sites: String,
    },
    /// The target-site request shape is outside unary forwarding v1.
    #[error("target site '{target_site_id}' is unsupported for this request: {reason}")]
    TargetSiteUnsupported {
        target_site_id: String,
        reason: String,
    },
    /// Target-site resolution or project binding failed before remote I/O.
    #[error("target site '{target_site_id}' resolution failed: {detail}")]
    TargetSiteResolutionFailed {
        target_site_id: String,
        detail: String,
    },
    /// Pull-back found local workspace changes since the remote push.
    #[error("target site '{target_site_id}' pull conflict: {detail}")]
    TargetSiteForwardConflict {
        target_site_id: String,
        detail: String,
    },
    /// Remote site, remote CAS, or returned remote snapshot failed.
    #[error("target site '{target_site_id}' remote failure: {detail}")]
    TargetSiteForwardBadGateway {
        target_site_id: String,
        detail: String,
    },
    /// Local forwarding orchestration failed unexpectedly.
    #[error("target site '{target_site_id}' forward failed internally: {detail}")]
    TargetSiteForwardInternal {
        target_site_id: String,
        detail: String,
    },
    /// Composed descriptor fails its `composed_value_contract`
    /// instance validation. This is a local preflight gate: a malformed
    /// descriptor must fail before any remote push, remote execute,
    /// or remote stream begins. Maps to 400 with a structured
    /// `details` envelope carrying per-field violations.
    #[error(
        "contract violation: `{canonical_ref}` ({error_count} errors, {warning_count} warnings)"
    )]
    ComposedValueContractViolation {
        canonical_ref: String,
        error_count: usize,
        warning_count: usize,
        details: ContractViolationDetails,
    },
    /// The daemon cannot prove that a previously reserved hook action is safe
    /// to issue again, or the durable identity/response record drifted. This is
    /// always fatal and non-retryable: retrying could duplicate an external
    /// side effect.
    #[error("hook dispatch integrity failure: {detail}")]
    HookDispatchIntegrity { detail: String },
    #[error("internal: {0}")]
    Internal(#[from] anyhow::Error),
}

/// Operator-actionable remediation for a missing required secret. The secret
/// resolves from any of the three sources (sealed vault, daemon host env,
/// `.env` overlay), so the remediation names all three. Shared by the dispatch
/// and resume paths so the wording cannot drift between them.
pub fn required_secret_remediation(env_var: &str) -> String {
    format!(
        "Set `{env_var}` via `ryeos vault set {env_var} <value>`, \
         a daemon/service environment variable, or a project/operator `.env`"
    )
}

impl DispatchError {
    /// Map the typed variant to the HTTP status `/execute` returns.
    /// The execute response mode calls this once per error path; status
    /// is determined by variant, never by matching the message string.
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::InvalidRef(..)
            | Self::AliasCycle { .. }
            | Self::AliasChainTooLong { .. }
            | Self::CapabilityRejected { .. }
            | Self::SchemaMisconfigured { .. }
            | Self::RootExecutorMissing { .. } => StatusCode::BAD_REQUEST,
            Self::RefBindingNotApplicable { .. } => StatusCode::BAD_REQUEST,
            Self::InsufficientCaps { .. }
            | Self::ServiceCapDenied { .. }
            | Self::MissingCap { .. }
            | Self::LaunchPolicyForbidden { .. } => StatusCode::FORBIDDEN,
            Self::NotFound
            | Self::ServiceNotInstalled { .. }
            | Self::LaunchResourceNotFound { .. } => StatusCode::NOT_FOUND,
            Self::NotRootExecutable { .. } | Self::StreamingNotImplemented => {
                StatusCode::NOT_IMPLEMENTED
            }
            // State-conflict: push-first, checkout race, etc.
            Self::ProjectSource(_)
            | Self::ProjectSourcePushFirst(_)
            | Self::Conflict(_)
            | Self::LaunchCancelled { .. } => StatusCode::CONFLICT,
            // Bad gateway: the daemon reached out to a subsystem
            // (service handler, runtime binary, CAS) and it was
            // missing, unavailable, or returned an error.
            Self::ServiceHandlerMissing { .. }
            | Self::ServiceUnavailable { .. }
            | Self::SubprocessExecutorMissing { .. }
            | Self::SubprocessRunFailed { .. }
            | Self::RuntimeMaterializationFailed { .. }
            | Self::RequiredSecretMissing { .. }
            | Self::ProjectSourceCheckoutFailed(_)
            | Self::MethodFailed { .. }
            | Self::MethodNotImplemented { .. } => StatusCode::BAD_GATEWAY,
            Self::UnknownMethod { .. }
            | Self::MethodInvalidArg { .. }
            | Self::ProjectionInvariant { .. }
            | Self::InvalidLaunchMode { .. }
            | Self::ComposedValueContractViolation { .. }
            | Self::UnknownTargetSite { .. }
            | Self::TargetSiteUnsupported { .. }
            | Self::TargetSiteResolutionFailed { .. } => StatusCode::BAD_REQUEST,
            Self::ExecutionNotRestartEligible { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::LaunchPreparationFailed { classification, .. } if classification == "caller" => {
                StatusCode::BAD_REQUEST
            }
            Self::LaunchPreparationFailed { classification, .. }
                if classification == "configuration" =>
            {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            Self::LaunchPreparationFailed { classification, .. }
                if classification == "unavailable" =>
            {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::LaunchPreparationFailed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Self::TargetSiteForwardConflict { .. } => StatusCode::CONFLICT,
            Self::ProtocolNotRegistered(_) | Self::TargetSiteForwardBadGateway { .. } => {
                StatusCode::BAD_GATEWAY
            }
            Self::StreamingNotDetachable => StatusCode::BAD_REQUEST,
            Self::HookDispatchIntegrity { .. }
            | Self::Internal(_)
            | Self::TargetSiteForwardInternal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Stable machine-readable error code for structured error surfaces.
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidRef(..) => "invalid_ref",
            Self::NotRootExecutable { .. } => "not_root_executable",
            Self::InsufficientCaps { .. } => "insufficient_caps",
            Self::AliasCycle { .. } => "alias_cycle",
            Self::AliasChainTooLong { .. } => "alias_chain_too_long",
            Self::SchemaMisconfigured { .. } => "schema_misconfigured",
            Self::CapabilityRejected { .. } => "capability_rejected",
            Self::StreamingNotImplemented => "streaming_not_implemented",
            Self::ProjectSource(_) => "project_source",
            Self::ServiceHandlerMissing { .. } => "service_handler_missing",
            Self::ServiceCapDenied { .. } => "service_cap_denied",
            Self::ServiceUnavailable { .. } => "service_unavailable",
            Self::ServiceNotInstalled { .. } => "service_not_installed",
            Self::SubprocessExecutorMissing { .. } => "subprocess_executor_missing",
            Self::RootExecutorMissing { .. } => "root_executor_missing",
            Self::SubprocessRunFailed { .. } => "subprocess_run_failed",
            Self::ExecutionNotRestartEligible { .. } => "execution_not_restart_eligible",
            Self::RuntimeMaterializationFailed { .. } => "runtime_materialization_failed",
            Self::LaunchCancelled { .. } => "launch_cancelled",
            Self::RequiredSecretMissing { .. } => "required_secret_missing",
            Self::LaunchPreparationFailed { code, .. } => code,
            Self::LaunchPolicyForbidden { code, .. }
            | Self::LaunchResourceNotFound { code, .. } => code,
            Self::RefBindingNotApplicable { .. } => "ref_binding_not_applicable",
            Self::ProjectSourcePushFirst(_) => "project_source_push_first",
            Self::ProjectSourceCheckoutFailed(_) => "project_source_checkout_failed",
            Self::MissingCap { .. } => "missing_cap",
            Self::NotFound => "not_found",
            Self::Conflict(_) => "conflict",
            Self::UnknownMethod { .. } => "unknown_method",
            Self::MethodInvalidArg { .. } => "method_invalid_arg",
            Self::MethodFailed { .. } => "method_failed",
            Self::MethodNotImplemented { .. } => "method_not_implemented",
            Self::ProjectionInvariant { .. } => "projection_invariant",
            Self::ProtocolNotRegistered(_) => "protocol_not_registered",
            Self::StreamingNotDetachable => "streaming_not_detachable",
            Self::InvalidLaunchMode { .. } => "invalid_launch_mode",
            Self::ComposedValueContractViolation { .. } => "contract_violation",
            Self::UnknownTargetSite { .. } => "unknown_target_site",
            Self::TargetSiteUnsupported { .. } => "target_site_unsupported",
            Self::TargetSiteResolutionFailed { .. } => "target_site_resolution_failed",
            Self::TargetSiteForwardConflict { .. } => "target_site_forward_conflict",
            Self::TargetSiteForwardBadGateway { .. } => "target_site_forward_bad_gateway",
            Self::TargetSiteForwardInternal { .. } => "target_site_forward_internal",
            Self::HookDispatchIntegrity { .. } => {
                ryeos_runtime::envelope::HOOK_INTEGRITY_FAILURE_CODE
            }
            Self::Internal(_) => "internal",
        }
    }

    /// Whether reissuing the same dispatch may succeed without an authored or
    /// configuration change. This is an explicit allowlist: unknown and newly
    /// added failures remain non-retryable until their safety is established.
    pub fn retryable(&self) -> bool {
        match self {
            Self::ServiceUnavailable { .. } => true,
            Self::LaunchPreparationFailed { classification, .. } => classification == "unavailable",
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn required_secret_missing_names_all_three_sources() {
        let err = DispatchError::RequiredSecretMissing {
            item_ref: "work:test/x".to_string(),
            env_var: "ZEN_API_KEY".to_string(),
            source_kind: "dependency".to_string(),
            source_name: "example".to_string(),
            remediation: required_secret_remediation("ZEN_API_KEY"),
        };
        let msg = err.to_string();
        assert!(msg.contains("sealed vault"), "got: {msg}");
        assert!(msg.contains("daemon host environment"), "got: {msg}");
        assert!(msg.contains(".env"), "got: {msg}");
        assert!(
            !msg.contains("set vault entry"),
            "stale wording leaked: {msg}"
        );

        let rem = required_secret_remediation("ZEN_API_KEY");
        assert!(
            rem.contains("ryeos vault set ZEN_API_KEY <value>"),
            "got: {rem}"
        );
        assert!(rem.contains("environment variable"), "got: {rem}");
        assert!(rem.contains(".env"), "got: {rem}");
        assert!(
            !rem.contains("vault put"),
            "stale vault command leaked: {rem}"
        );
        assert!(
            !rem.contains("ryeos-core-tools"),
            "stale binary leaked: {rem}"
        );
    }

    fn sample_details() -> ContractViolationDetails {
        ContractViolationDetails {
            errors: vec![ContractViolationEntry {
                path: "launch.mode".to_string(),
                code: "enum_mismatch".to_string(),
                expected: "\"wait\" | \"detached\"".to_string(),
                found: "\"bogus\"".to_string(),
            }],
            warnings: vec![],
        }
    }

    #[test]
    fn conflict_maps_to_409_with_code() {
        let e = DispatchError::Conflict(
            "schedule 'snap-track-feed' in this project is registered by a different \
             principal; deregister it on the remote or run the sync as its owner"
                .to_string(),
        );
        assert_eq!(e.http_status(), StatusCode::CONFLICT);
        assert_eq!(e.code(), "conflict");
        assert!(e.to_string().contains("snap-track-feed"));
    }

    #[test]
    fn hook_integrity_is_internal_and_never_retryable() {
        let error = DispatchError::HookDispatchIntegrity {
            detail: "pending outcome unknown".to_string(),
        };
        assert_eq!(error.http_status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error.code(),
            ryeos_runtime::envelope::HOOK_INTEGRITY_FAILURE_CODE
        );
        assert!(!error.retryable());
    }

    #[test]
    fn service_not_installed_maps_to_404_with_code() {
        let e = DispatchError::ServiceNotInstalled {
            service_ref: "service:scheduler/register".to_string(),
            installed_bundles: vec!["core".to_string(), "hosted-node".to_string()],
            searched_spaces: vec!["project".to_string(), "bundle".to_string()],
        };
        assert_eq!(e.http_status(), StatusCode::NOT_FOUND);
        assert_eq!(e.code(), "service_not_installed");
        let msg = e.to_string();
        assert!(
            msg.contains("service:scheduler/register"),
            "must name the ref, got: {msg}"
        );
        assert!(
            msg.contains("core, hosted-node"),
            "must list installed bundles, got: {msg}"
        );
    }

    #[test]
    fn restart_ineligible_execution_is_typed_and_non_retryable() {
        let error = DispatchError::ExecutionNotRestartEligible {
            item_ref: "tool:test/node-policy".to_string(),
            reason: "node policy is mutable".to_string(),
            remediation: "use verified content".to_string(),
        };
        assert_eq!(error.code(), "execution_not_restart_eligible");
        assert_eq!(error.http_status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!error.retryable());
    }

    #[test]
    fn launch_cancellation_has_stable_conflict_contract() {
        let error = DispatchError::LaunchCancelled {
            stage: "irreversible thread handoff",
        };
        assert_eq!(error.code(), "launch_cancelled");
        assert_eq!(error.http_status(), StatusCode::CONFLICT);
        assert!(!error.retryable());
    }

    #[test]
    fn contract_violation_http_status_is_bad_request() {
        let e = DispatchError::ComposedValueContractViolation {
            canonical_ref: "work:foo/bar".to_string(),
            error_count: 1,
            warning_count: 0,
            details: sample_details(),
        };
        assert_eq!(e.http_status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn contract_violation_code_is_contract_violation() {
        let e = DispatchError::ComposedValueContractViolation {
            canonical_ref: "work:foo/bar".to_string(),
            error_count: 1,
            warning_count: 0,
            details: sample_details(),
        };
        assert_eq!(e.code(), "contract_violation");
    }

    #[test]
    fn contract_violation_display_includes_ref_and_counts() {
        let e = DispatchError::ComposedValueContractViolation {
            canonical_ref: "work:foo/bar".to_string(),
            error_count: 2,
            warning_count: 1,
            details: sample_details(),
        };
        let msg = e.to_string();
        assert!(msg.contains("work:foo/bar"), "must include ref, got: {msg}");
        assert!(
            msg.contains("2 errors"),
            "must include error count, got: {msg}"
        );
        assert!(
            msg.contains("1 warning"),
            "must include warning count, got: {msg}"
        );
    }

    #[test]
    fn contract_violation_details_from_report() {
        use ryeos_engine::contracts::{
            InstanceValidationReport, InstanceViolation, InstanceViolationCode,
        };

        let report = InstanceValidationReport {
            errors: vec![InstanceViolation {
                path: "launch.mode".to_string(),
                code: InstanceViolationCode::EnumMismatch,
                expected: "\"wait\"".to_string(),
                found: "\"detached\"".to_string(),
            }],
            warnings: vec![InstanceViolation {
                path: "extra".to_string(),
                code: InstanceViolationCode::UnexpectedField,
                expected: "<none>".to_string(),
                found: "value".to_string(),
            }],
        };

        let details = ContractViolationDetails::from_report(&report);
        assert_eq!(details.errors.len(), 1);
        assert_eq!(details.warnings.len(), 1);
        assert_eq!(details.errors[0].path, "launch.mode");
        assert_eq!(details.errors[0].code, "enum_mismatch");
        assert_eq!(details.warnings[0].code, "unexpected_field");
    }
}
