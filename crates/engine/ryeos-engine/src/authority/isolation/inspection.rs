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
    pub trusted_exclusive_session: ProtocolProcessControlReadiness,
}

impl ProcessScopeReadiness {
    /// `required` is the per-scope part of the node's declared capability
    /// floor; a capability the policy does not name is not demanded here.
    pub(crate) fn classify(
        policy: IsolationProcessScopePolicy,
        required: BTreeSet<lillux::ProcessScopeCapability>,
        trusted_process_group_sessions: bool,
        authority_digest: Option<String>,
        qualified_capabilities: BTreeSet<lillux::ProcessScopeCapability>,
    ) -> Self {
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
            trusted_exclusive_session: ProtocolProcessControlReadiness {
                ready: trusted_process_group_sessions,
                reason: if trusted_process_group_sessions {
                    ProcessControlReadinessReason::TrustedProcessGroup
                } else {
                    ProcessControlReadinessReason::PolicyUnconfigured
                },
            },
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
    TrustedProcessGroup,
}

impl ProcessControlReadinessReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Ready => "ready",
            Self::PolicyUnconfigured => "policy_unconfigured",
            Self::ProtectedAuthorityAbsent => "protected_authority_absent",
            Self::RequiredCapabilitiesMissing => "required_capabilities_missing",
            Self::TrustedProcessGroup => "trusted_process_group",
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
    /// The host class the deployment declared at initialization, whose
    /// floor and isolation lane this policy carries.
    pub host_class: String,
    pub mode: IsolationMode,
    pub digest: Option<String>,
    pub backend: IsolationBackendInspection,
    pub process_scopes: IsolationProcessScopePolicy,
    /// Signed capability floor this node must prove before it starts.
    pub requires_capabilities: Vec<lillux::process_control::CapabilityRequirement>,
    /// Signed executable names of inits trusted to exit with their only
    /// child, which can make the node's exit end its container enclosure.
    pub enclosure_inits: Vec<String>,
    /// Signed hard lifetime of the node's host class, when declared.
    pub host_lifetime_bound: Option<lillux::process_control::HostLifetimeBound>,
    /// Populated only after the retained provider's actual placement/barrier/
    /// termination probe succeeded, never from a list of implemented features.
    pub process_scope_capabilities: BTreeSet<lillux::ProcessScopeCapability>,
    pub process_scope_readiness: ProcessScopeReadiness,
    pub filesystem: IsolationFilesystemPolicy,
    pub network: IsolationNetworkPolicy,
    pub environment: IsolationEnvironmentPolicy,
    pub limits: IsolationLimitsPolicy,
}

impl IsolationInspection {
    /// Node permission to grant scoped launches a nested sandbox, read from
    /// the signed floor and proc ceiling exactly as the policy derives it.
    pub fn nested_sandbox(&self) -> bool {
        super::policy::nested_sandbox_from(
            &self.requires_capabilities,
            self.filesystem.proc_filesystem,
        )
    }
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

    fn floor() -> BTreeSet<lillux::ProcessScopeCapability> {
        [
            lillux::ProcessScopeCapability::Termination,
            lillux::ProcessScopeCapability::Recovery,
        ]
        .into()
    }

    #[test]
    fn protected_authority_does_not_override_unconfigured_policy() {
        let readiness = ProcessScopeReadiness::classify(
            IsolationProcessScopePolicy::Unconfigured {},
            BTreeSet::new(),
            false,
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
        let readiness = ProcessScopeReadiness::classify(
            required_policy(),
            floor(),
            false,
            None,
            BTreeSet::new(),
        );
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
            floor(),
            false,
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
    fn quiescence_is_probed_only_when_wanted() {
        // The wanted set handed to the provider is the floor's per-scope
        // part; a floor that does not name quiescence:scope asks for no
        // freeze, and readiness does not miss what was never wanted.
        let mut policy = super::super::IsolationPolicy::disabled_for_authoring();
        policy.process_scopes = required_policy();
        policy.requires_capabilities = vec!["recovery:scope".into(), "termination:scope".into()];
        let wanted = policy.required_process_scope_capabilities();
        assert_eq!(wanted, floor());
        assert!(!wanted.contains(&lillux::ProcessScopeCapability::Quiescence));
        policy.requires_capabilities = vec![
            "quiescence:scope".into(),
            "recovery:scope".into(),
            "termination:scope".into(),
        ];
        assert_eq!(
            policy.required_process_scope_capabilities(),
            complete_capabilities()
        );

        let without_freeze: BTreeSet<_> = [
            lillux::ProcessScopeCapability::Termination,
            lillux::ProcessScopeCapability::Recovery,
        ]
        .into();
        let digest = Some(format!("sha256:{}", "c".repeat(64)));
        let ready = ProcessScopeReadiness::classify(
            required_policy(),
            floor(),
            false,
            digest.clone(),
            without_freeze.clone(),
        );
        assert!(ready.exclusive_session.ready);
        let mut with_freeze = floor();
        with_freeze.insert(lillux::ProcessScopeCapability::Quiescence);
        let refused = ProcessScopeReadiness::classify(
            required_policy(),
            with_freeze,
            false,
            digest,
            without_freeze,
        );
        assert_eq!(
            refused.exclusive_session.reason,
            ProcessControlReadinessReason::RequiredCapabilitiesMissing
        );
    }

    #[test]
    fn trusted_process_group_readiness_is_explicit_and_distinct() {
        let readiness = ProcessScopeReadiness::classify(
            IsolationProcessScopePolicy::Unconfigured {},
            BTreeSet::new(),
            true,
            None,
            BTreeSet::new(),
        );
        assert!(readiness.trusted_exclusive_session.ready);
        assert_eq!(
            readiness.trusted_exclusive_session.reason,
            ProcessControlReadinessReason::TrustedProcessGroup
        );
        assert!(!readiness.exclusive_session.ready);
    }

    #[test]
    fn readiness_reason_text_matches_its_wire_value() {
        for reason in [
            ProcessControlReadinessReason::NotRequired,
            ProcessControlReadinessReason::Ready,
            ProcessControlReadinessReason::PolicyUnconfigured,
            ProcessControlReadinessReason::ProtectedAuthorityAbsent,
            ProcessControlReadinessReason::RequiredCapabilitiesMissing,
            ProcessControlReadinessReason::TrustedProcessGroup,
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap(), reason.as_str());
        }
    }
}
