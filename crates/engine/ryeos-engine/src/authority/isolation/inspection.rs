use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use ryeos_isolation_protocol::{
    InspectedArtifact, IsolationArtifactRole, IsolationBackendSelection, IsolationCapability,
};
use serde::{Deserialize, Serialize};

use super::{
    IsolationEnvironmentPolicy, IsolationFilesystemPolicy, IsolationLimitsPolicy, IsolationMode,
    IsolationNetworkPolicy, IsolationProcessScopePolicy,
};

/// Read-only evidence for the node's protected process-control authority.
///
/// Signed node policy and protected host authority are independent activation
/// inputs. This reports both without exposing the native delegation path.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProcessScopeReadiness {
    pub policy: IsolationProcessScopePolicy,
    pub authority: ProcessScopeAuthorityStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority_digest: Option<String>,
    pub qualified_capabilities: BTreeSet<lillux::ProcessScopeCapability>,
    /// These modes do not require exclusive process-scope lifecycle control.
    /// Their remaining program/environment requirements are admitted elsewhere.
    pub ordinary_subprocess: ProtocolProcessControlReadiness,
    pub pooled_requests: ProtocolProcessControlReadiness,
    pub exclusive_session: ProtocolProcessControlReadiness,
}

impl ProcessScopeReadiness {
    pub(crate) fn classify(
        policy: IsolationProcessScopePolicy,
        authority_digest: Option<String>,
        qualified_capabilities: BTreeSet<lillux::ProcessScopeCapability>,
    ) -> Self {
        let required: BTreeSet<_> = [
            lillux::ProcessScopeCapability::Quiescence,
            lillux::ProcessScopeCapability::Termination,
            lillux::ProcessScopeCapability::Recovery,
        ]
        .into();
        let (authority, ready, reason) = match (&policy, authority_digest.is_some()) {
            (IsolationProcessScopePolicy::Unconfigured {}, false) => (
                ProcessScopeAuthorityStatus::Absent,
                false,
                ProcessControlReadinessReason::PolicyUnconfigured,
            ),
            (IsolationProcessScopePolicy::Unconfigured {}, true) => (
                ProcessScopeAuthorityStatus::PresentUnselected,
                false,
                ProcessControlReadinessReason::PolicyUnconfigured,
            ),
            (IsolationProcessScopePolicy::Required { .. }, false) => (
                ProcessScopeAuthorityStatus::Absent,
                false,
                ProcessControlReadinessReason::ProtectedAuthorityAbsent,
            ),
            (IsolationProcessScopePolicy::Required { .. }, true)
                if qualified_capabilities.is_superset(&required) =>
            {
                (
                    ProcessScopeAuthorityStatus::Qualified,
                    true,
                    ProcessControlReadinessReason::Ready,
                )
            }
            (IsolationProcessScopePolicy::Required { .. }, true) => (
                ProcessScopeAuthorityStatus::PresentUnqualified,
                false,
                ProcessControlReadinessReason::RequiredCapabilitiesMissing,
            ),
        };
        Self {
            policy,
            authority,
            authority_digest,
            qualified_capabilities,
            ordinary_subprocess: ProtocolProcessControlReadiness {
                ready: true,
                reason: ProcessControlReadinessReason::NotRequired,
            },
            pooled_requests: ProtocolProcessControlReadiness {
                ready: true,
                reason: ProcessControlReadinessReason::NotRequired,
            },
            exclusive_session: ProtocolProcessControlReadiness { ready, reason },
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessScopeAuthorityStatus {
    Absent,
    PresentUnselected,
    PresentUnqualified,
    Qualified,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProtocolProcessControlReadiness {
    pub ready: bool,
    pub reason: ProcessControlReadinessReason,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessControlReadinessReason {
    NotRequired,
    Ready,
    PolicyUnconfigured,
    ProtectedAuthorityAbsent,
    RequiredCapabilitiesMissing,
}

impl ProcessControlReadinessReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Ready => "ready",
            Self::PolicyUnconfigured => "policy_unconfigured",
            Self::ProtectedAuthorityAbsent => "protected_authority_absent",
            Self::RequiredCapabilitiesMissing => "required_capabilities_missing",
        }
    }
}

/// Backend resolution facts and the exact policy snapshot used by a runtime.
///
/// Doctor and status surfaces consume this value rather than reparsing the
/// source file with a second implementation. Enforced policy loading captures
/// the configured backend immediately. Disabled policy never resolves or
/// probes a backend.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IsolationInspection {
    pub source: Option<PathBuf>,
    pub version: u32,
    pub mode: IsolationMode,
    pub digest: Option<String>,
    pub backend: IsolationBackendInspection,
    pub process_scopes: IsolationProcessScopePolicy,
    /// Populated only after the retained provider's actual placement/barrier/
    /// termination probe succeeded, never from a list of implemented features.
    pub process_scope_capabilities: BTreeSet<lillux::ProcessScopeCapability>,
    pub process_scope_readiness: ProcessScopeReadiness,
    pub filesystem: IsolationFilesystemPolicy,
    pub network: IsolationNetworkPolicy,
    pub environment: IsolationEnvironmentPolicy,
    pub limits: IsolationLimitsPolicy,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IsolationBackendInspection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<IsolationBackendSelection>,
    pub status: IsolationBackendStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_manifest_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signer_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter_build: Option<String>,
    pub declared_capabilities: BTreeSet<IsolationCapability>,
    pub effective_capabilities: BTreeSet<IsolationCapability>,
    pub artifacts: BTreeMap<IsolationArtifactRole, InspectedArtifact>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IsolationBackendStatus {
    Disabled,
    Available,
    Unavailable,
    Incompatible,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required_policy() -> IsolationProcessScopePolicy {
        IsolationProcessScopePolicy::Required {
            control_timeout_ms: 1_000,
            nested_sandbox: false,
        }
    }

    fn complete_capabilities() -> BTreeSet<lillux::ProcessScopeCapability> {
        [
            lillux::ProcessScopeCapability::Quiescence,
            lillux::ProcessScopeCapability::Termination,
            lillux::ProcessScopeCapability::Recovery,
        ]
        .into()
    }

    #[test]
    fn protected_authority_does_not_override_unconfigured_policy() {
        let readiness = ProcessScopeReadiness::classify(
            IsolationProcessScopePolicy::Unconfigured {},
            Some(format!("sha256:{}", "a".repeat(64))),
            BTreeSet::new(),
        );
        assert_eq!(
            readiness.authority,
            ProcessScopeAuthorityStatus::PresentUnselected
        );
        assert!(!readiness.exclusive_session.ready);
        assert_eq!(
            readiness.exclusive_session.reason,
            ProcessControlReadinessReason::PolicyUnconfigured
        );
        assert!(readiness.pooled_requests.ready);
        assert_eq!(
            readiness.pooled_requests.reason,
            ProcessControlReadinessReason::NotRequired
        );
    }

    #[test]
    fn required_policy_without_protected_authority_is_not_ready() {
        let readiness = ProcessScopeReadiness::classify(required_policy(), None, BTreeSet::new());
        assert_eq!(readiness.authority, ProcessScopeAuthorityStatus::Absent);
        assert!(!readiness.exclusive_session.ready);
        assert_eq!(
            readiness.exclusive_session.reason,
            ProcessControlReadinessReason::ProtectedAuthorityAbsent
        );
    }

    #[test]
    fn required_policy_and_qualified_authority_are_ready() {
        let readiness = ProcessScopeReadiness::classify(
            required_policy(),
            Some(format!("sha256:{}", "b".repeat(64))),
            complete_capabilities(),
        );
        assert_eq!(readiness.authority, ProcessScopeAuthorityStatus::Qualified);
        assert!(readiness.exclusive_session.ready);
        assert_eq!(
            readiness.exclusive_session.reason,
            ProcessControlReadinessReason::Ready
        );
    }

    #[test]
    fn readiness_reason_text_matches_its_wire_value() {
        for reason in [
            ProcessControlReadinessReason::NotRequired,
            ProcessControlReadinessReason::Ready,
            ProcessControlReadinessReason::PolicyUnconfigured,
            ProcessControlReadinessReason::ProtectedAuthorityAbsent,
            ProcessControlReadinessReason::RequiredCapabilitiesMissing,
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap(), reason.as_str());
        }
    }
}
