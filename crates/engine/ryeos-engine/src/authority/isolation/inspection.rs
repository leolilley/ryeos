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
    /// Which mechanisms this node has qualified to contain and settle every
    /// descendant of an execution, including those that leave its process
    /// group. Derived from qualified facts, never from configuration alone.
    pub descendant_containment: DescendantContainmentReadiness,
}

/// Result of the host's PID-namespace containment qualification probe.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PidNamespaceContainmentQualification {
    pub qualified: bool,
    /// Grade of the proof: measured on a native kernel, provider-attested
    /// where the kernel is emulated. A floor decides whether that grade is
    /// accepted.
    pub source: lillux::process_control::ProofSource,
    /// Why the mechanism is or is not qualified; always set.
    pub detail: String,
}

impl PidNamespaceContainmentQualification {
    /// Provider name this proof is attributed to in the capability report
    /// and judged against a floor.
    pub const PROVIDER: &'static str = lillux::process_control::PID_NAMESPACE_PROVIDER;

    pub fn not_probed(reason: impl Into<String>) -> Self {
        Self {
            qualified: false,
            source: lillux::process_control::ProofSource::Measured,
            detail: reason.into(),
        }
    }

    pub fn qualified_by(
        source: lillux::process_control::ProofSource,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            qualified: true,
            source,
            detail: detail.into(),
        }
    }

    /// Qualify the kernel mechanism the Worker launch relies on: unshare of
    /// `CLONE_NEWUSER | CLONE_NEWPID`, a retained namespace init, and the
    /// kernel ending every member (detached, double-forked and nested-PID-
    /// namespace descendants included) with that init. Run once per boot per
    /// process; the same answer serves every runtime generation.
    pub fn probe() -> Self {
        match lillux::process_control::cached_pid_namespace_containment_probe() {
            lillux::process_control::ProbeOutcome::Allowed => Self::qualified_by(
                lillux::process_control::pid_namespace_proof_source(
                    lillux::process_control::host_kernel_is_gvisor(),
                ),
                "kernel mechanism qualified (CLONE_NEWUSER|CLONE_NEWPID): a retained \
                 namespace init contained and settled setsid, double-fork and nested \
                 PID namespace descendants in a disposable probe",
            ),
            other => Self::not_probed(format!(
                "PID-namespace containment probe (CLONE_NEWUSER|CLONE_NEWPID): {other:?}"
            )),
        }
    }

    /// Apply the node's capability floor exactly as the capability report
    /// grades this proof. A floor entry naming the PID-namespace termination
    /// or recovery capability must be satisfied by this proof's grade and
    /// provider. Where the floor does not name it, only a measured proof
    /// counts: an attested one is admitted solely by a floor that accepts
    /// that grade by name.
    pub fn graded_by(mut self, floor: &[lillux::process_control::CapabilityRequirement]) -> Self {
        use lillux::process_control::{CapabilityEvidence, ProofSource, satisfies};
        if !self.qualified {
            return self;
        }
        let evidence = CapabilityEvidence {
            source: self.source.clone(),
            provider: Self::PROVIDER.to_owned(),
            basis: self.detail.clone(),
        };
        let mut named = false;
        for entry in floor {
            for name in entry.names() {
                if name == lillux::process_control::TERMINATION_PID_NAMESPACE
                    || name == lillux::process_control::RECOVERY_PID_NAMESPACE
                {
                    named = true;
                    if let Err(reason) = satisfies(entry, name, &evidence) {
                        self.qualified = false;
                        self.detail = reason;
                        return self;
                    }
                }
            }
        }
        if !named && self.source != ProofSource::Measured {
            self.qualified = false;
            self.detail = format!(
                "the {} proof of PID-namespace containment is admitted only by a floor that names it and accepts that grade",
                self.source.as_str()
            );
        }
        self
    }
}

/// The mechanisms by which this node can retain authority over every
/// descendant of one execution.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DescendantContainmentReadiness {
    /// A qualified cgroup scope provider (termination and recovery).
    pub cgroup_scope: bool,
    pub pid_namespace: PidNamespaceContainmentQualification,
    /// The node policy accepts the trusted process-group lane. This is a
    /// weaker guarantee that lets a descendant escape and is never an
    /// automatic fallback.
    pub trusted_process_group: bool,
}

/// The strongest guarantee answering a declared descendant requirement.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DescendantContainmentGuarantee {
    CgroupScope,
    PidNamespaceInit,
    /// Weaker: trusted controller process-group session, no escape proof.
    TrustedProcessGroup,
}

impl DescendantContainmentReadiness {
    /// A guarantee which contains and settles descendants that leave the
    /// process group, from qualified facts. Preference is stable: a
    /// qualified cgroup scope, then the PID-namespace init.
    pub fn enforced_guarantee(&self) -> Option<DescendantContainmentGuarantee> {
        if self.cgroup_scope {
            Some(DescendantContainmentGuarantee::CgroupScope)
        } else if self.pid_namespace.qualified {
            Some(DescendantContainmentGuarantee::PidNamespaceInit)
        } else {
            None
        }
    }
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
        pid_namespace: PidNamespaceContainmentQualification,
    ) -> Self {
        let cgroup_scope = matches!(policy, IsolationProcessScopePolicy::Required { .. })
            && authority_digest.is_some()
            && qualified_capabilities.contains(&lillux::ProcessScopeCapability::Termination)
            && qualified_capabilities.contains(&lillux::ProcessScopeCapability::Recovery);
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
            descendant_containment: DescendantContainmentReadiness {
                cgroup_scope,
                pid_namespace,
                trusted_process_group: trusted_process_group_sessions,
            },
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

/// The node runs descendant-contained, descriptor-mounted isolation.
pub const ISOLATION_ENFORCED: &str = "isolation:enforced";
/// A tool authored `filesystem_authority: captured_execution` is admissible:
/// isolation is enforced and the node's filesystem ceilings admit
/// `{verified_code}` readable and `{project}` access.
pub const FILESYSTEM_CAPTURED_EXECUTION: &str = "filesystem:captured_execution";
/// A captured tool may hold the captured-compatible callback class. This is
/// node-level only; each launch still checks its protocol declares the class.
pub const CALLBACK_CAPTURED_COMPATIBLE: &str = "callback:captured_compatible";
/// Node policy accepts trusted process-group sessions on a node whose
/// isolation is not enforced; a weaker guarantee, never an automatic fallback.
pub const SESSIONS_TRUSTED_PROCESS_GROUP: &str = "sessions:trusted_process_group";
/// Provider id of guarantees this node derives from its own isolation state.
pub const ISOLATION_PROVIDER: &str = "isolation";
/// Provider id of the guarantee node policy grants.
pub const NODE_POLICY_PROVIDER: &str = "node_policy";

/// What the report and variant selection both need to know about the host to
/// grade a guarantee. Nothing else decides a grade.
#[derive(Debug, Clone, Default)]
pub struct GuaranteeHost {
    /// The kernel is emulated, so mechanism proofs are provider-attested.
    pub gvisor: bool,
    /// Provider id the delegated cgroup scopes are attributed to.
    pub scope_provider: String,
    /// Why no cgroup scope can be opened here, when none can.
    pub scope_absent: Option<String>,
}

impl GuaranteeHost {
    /// The host as this process observes it.
    pub fn observe() -> Self {
        Self {
            gvisor: lillux::process_control::host_kernel_is_gvisor(),
            ..Self::default()
        }
    }

    /// Grade of a mechanism qualified by a probe: measured on a native
    /// kernel, provider-attested where the kernel is emulated.
    pub fn mechanism_source(&self) -> lillux::process_control::ProofSource {
        lillux::process_control::pid_namespace_proof_source(self.gvisor)
    }

    /// Grade of a mechanism accepted by policy rather than enforced.
    pub fn accepted_source(&self) -> lillux::process_control::ProofSource {
        if self.gvisor {
            lillux::process_control::ProofSource::ProviderAttested
        } else {
            lillux::process_control::ProofSource::Trusted
        }
    }
}

/// Record one per-scope guarantee. The one place a scope capability is
/// provided or refused, for the capability report and for variant selection.
pub fn provide_scope_guarantee(
    view: &mut lillux::process_control::HostCapabilities,
    capability: lillux::ProcessScopeCapability,
    qualified: bool,
    host: &GuaranteeHost,
    refusal: &str,
) {
    let name = capability.capability_name();
    if qualified {
        view.provide(
            name,
            host.mechanism_source(),
            &host.scope_provider,
            "qualified by a disposable probe scope of the delegated cgroup provider",
        );
    } else {
        view.refuse(name, refusal);
    }
}

/// Descendant containment by the execution's retained PID-namespace init.
/// Provided only when the enforced runtime's disposable probe showed that a
/// setsid'd, double-forked and nested-namespace descendant all die with the
/// init and the init's exit settles them. Quiescence is explicitly never
/// provided: namespace membership is not an atomic writer freeze.
pub fn add_pid_namespace_containment(
    view: &mut lillux::process_control::HostCapabilities,
    containment: &PidNamespaceContainmentQualification,
) {
    use lillux::process_control::{
        QUIESCENCE_PID_NAMESPACE, RECOVERY_PID_NAMESPACE, TERMINATION_PID_NAMESPACE,
    };
    for name in [TERMINATION_PID_NAMESPACE, RECOVERY_PID_NAMESPACE] {
        if containment.qualified {
            view.provide(
                name,
                containment.source.clone(),
                PidNamespaceContainmentQualification::PROVIDER,
                containment.detail.clone(),
            );
        } else {
            view.refuse(name, containment.detail.clone());
        }
    }
    view.refuse(
        QUIESCENCE_PID_NAMESPACE,
        "a retained PID-namespace init terminates and settles descendants but cannot freeze them",
    );
}

impl IsolationInspection {
    /// Whether the node's filesystem ceilings admit captured execution, as
    /// launch admission requires: `{verified_code}` readable and `{project}`
    /// writable, or readable for a read-only project.
    pub fn admits_captured_filesystem(&self) -> bool {
        let has = |list: &[String], entry: &str| list.iter().any(|item| item == entry);
        has(&self.filesystem.readable, "{verified_code}")
            && (has(&self.filesystem.writable, "{project}")
                || has(&self.filesystem.readable, "{project}"))
    }

    /// The guarantees isolation state and node policy decide, under the open
    /// capability names the node reports.
    pub fn add_policy_guarantees(
        &self,
        enforced: bool,
        host: &GuaranteeHost,
        view: &mut lillux::process_control::HostCapabilities,
    ) {
        use lillux::process_control::ProofSource;
        let captured = enforced && self.admits_captured_filesystem();
        let decisions = [
            (
                ISOLATION_ENFORCED,
                enforced,
                "descriptor-mounted isolation is enforced",
                "isolation is not enforced on this node",
            ),
            (
                FILESYSTEM_CAPTURED_EXECUTION,
                captured,
                "isolation is enforced and node ceilings admit {verified_code} readable and {project} access",
                if enforced {
                    "node filesystem ceilings do not admit {verified_code} readable and {project} access"
                } else {
                    "isolation is not enforced on this node"
                },
            ),
            (
                CALLBACK_CAPTURED_COMPATIBLE,
                captured,
                "node-level: captured execution is admissible; each launch still requires its protocol to declare the class",
                "captured execution is not admissible on this node",
            ),
        ];
        for (name, held, basis, refusal) in decisions {
            if held {
                view.provide(name, ProofSource::Measured, ISOLATION_PROVIDER, basis);
            } else {
                view.refuse(name, refusal);
            }
        }
        let trusted = self
            .process_scope_readiness
            .descendant_containment
            .trusted_process_group;
        match (trusted, enforced) {
            (true, false) => view.provide(
                SESSIONS_TRUSTED_PROCESS_GROUP,
                host.accepted_source(),
                NODE_POLICY_PROVIDER,
                "node policy accepts trusted process-group sessions; a descendant that leaves \
                 the process group is not contained",
            ),
            (true, true) => view.refuse(
                SESSIONS_TRUSTED_PROCESS_GROUP,
                "isolation is enforced, so sessions run descendant containment, not the trusted \
                 process-group lane",
            ),
            (false, _) => view.refuse(
                SESSIONS_TRUSTED_PROCESS_GROUP,
                "node policy does not accept trusted process-group sessions",
            ),
        }
    }

    /// Every guarantee this node reports for descendant containment, isolation
    /// and node policy, graded from `host`. The signed capability report and
    /// variant selection both take their grades from this one function; the
    /// report adds the host-only facts (quiescence, limits, residency).
    pub fn reported_guarantees(
        &self,
        enforced: bool,
        host: &GuaranteeHost,
    ) -> lillux::process_control::HostCapabilities {
        let mut view = lillux::process_control::HostCapabilities::default();
        self.add_policy_guarantees(enforced, host, &mut view);
        let containment = &self.process_scope_readiness.descendant_containment;
        let refusal = host
            .scope_absent
            .as_deref()
            .unwrap_or("no qualified cgroup scope provider on this node");
        for capability in [
            lillux::ProcessScopeCapability::Termination,
            lillux::ProcessScopeCapability::Recovery,
        ] {
            provide_scope_guarantee(
                &mut view,
                capability,
                containment.cgroup_scope,
                host,
                refusal,
            );
        }
        add_pid_namespace_containment(&mut view, &containment.pid_namespace);
        view
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
            PidNamespaceContainmentQualification::not_probed("test"),
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
            PidNamespaceContainmentQualification::not_probed("test"),
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
            PidNamespaceContainmentQualification::not_probed("test"),
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
            PidNamespaceContainmentQualification::not_probed("test"),
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
            PidNamespaceContainmentQualification::not_probed("test"),
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
            PidNamespaceContainmentQualification::not_probed("test"),
        );
        assert!(readiness.trusted_exclusive_session.ready);
        assert_eq!(
            readiness.trusted_exclusive_session.reason,
            ProcessControlReadinessReason::TrustedProcessGroup
        );
        assert!(!readiness.exclusive_session.ready);
    }

    #[test]
    fn descendant_containment_is_derived_from_qualified_facts_in_a_stable_preference() {
        let digest = || Some(format!("sha256:{}", "c".repeat(64)));
        let qualified = || {
            PidNamespaceContainmentQualification::qualified_by(
                lillux::process_control::ProofSource::Measured,
                "probed".to_owned(),
            )
        };
        let classify = |policy, caps: BTreeSet<_>, authority, pid_namespace| {
            ProcessScopeReadiness::classify(
                policy,
                BTreeSet::new(),
                false,
                authority,
                caps,
                pid_namespace,
            )
            .descendant_containment
        };
        // Termination and recovery suffice for containment: a cgroup
        // provider that did not qualify the freeze still contains.
        let cgroup = classify(
            required_policy(),
            [
                lillux::ProcessScopeCapability::Termination,
                lillux::ProcessScopeCapability::Recovery,
            ]
            .into(),
            digest(),
            PidNamespaceContainmentQualification::not_probed("refused"),
        );
        assert!(cgroup.cgroup_scope && !cgroup.pid_namespace.qualified);
        assert_eq!(
            cgroup.enforced_guarantee(),
            Some(DescendantContainmentGuarantee::CgroupScope)
        );
        // An unqualified or absent provider is never containment.
        for (policy, caps, authority) in [
            (
                required_policy(),
                [lillux::ProcessScopeCapability::Termination].into(),
                digest(),
            ),
            (required_policy(), complete_capabilities(), None),
            (
                IsolationProcessScopePolicy::Unconfigured {},
                complete_capabilities(),
                digest(),
            ),
        ] {
            let readiness = classify(
                policy,
                caps,
                authority,
                PidNamespaceContainmentQualification::not_probed("refused"),
            );
            assert!(!readiness.cgroup_scope);
            assert_eq!(readiness.enforced_guarantee(), None);
        }
        // The PID-namespace init answers when no scope provider does.
        let namespace = classify(
            IsolationProcessScopePolicy::Unconfigured {},
            BTreeSet::new(),
            None,
            qualified(),
        );
        assert_eq!(
            namespace.enforced_guarantee(),
            Some(DescendantContainmentGuarantee::PidNamespaceInit)
        );
        // Both qualified: the scope is preferred; the trusted lane is never
        // a guarantee of its own.
        let both = classify(
            required_policy(),
            complete_capabilities(),
            digest(),
            qualified(),
        );
        assert_eq!(
            both.enforced_guarantee(),
            Some(DescendantContainmentGuarantee::CgroupScope)
        );
        let trusted_only = ProcessScopeReadiness::classify(
            IsolationProcessScopePolicy::Unconfigured {},
            BTreeSet::new(),
            true,
            None,
            BTreeSet::new(),
            PidNamespaceContainmentQualification::not_probed("refused"),
        )
        .descendant_containment;
        assert!(trusted_only.trusted_process_group);
        assert_eq!(trusted_only.enforced_guarantee(), None);
    }

    #[test]
    fn reported_guarantees_follow_isolation_state_containment_and_node_policy() {
        let digest = || Some(format!("sha256:{}", "c".repeat(64)));
        let pid_namespace = || {
            PidNamespaceContainmentQualification::qualified_by(
                lillux::process_control::ProofSource::Measured,
                "probed".to_owned(),
            )
        };
        let inspection = |trusted, contained: bool| {
            let mut inspection = crate::isolation::IsolationRuntime::disabled_for_authoring()
                .inspection()
                .clone();
            inspection.filesystem.readable = vec!["{verified_code}".into()];
            inspection.filesystem.writable = vec!["{project}".into()];
            inspection.process_scope_readiness = if contained {
                ProcessScopeReadiness::classify(
                    required_policy(),
                    BTreeSet::new(),
                    trusted,
                    digest(),
                    complete_capabilities(),
                    pid_namespace(),
                )
            } else {
                ProcessScopeReadiness::classify(
                    IsolationProcessScopePolicy::Unconfigured {},
                    BTreeSet::new(),
                    trusted,
                    None,
                    BTreeSet::new(),
                    PidNamespaceContainmentQualification::not_probed("refused"),
                )
            };
            inspection
        };
        let native = GuaranteeHost::default();
        let provided = |view: &lillux::process_control::HostCapabilities| {
            view.provided.keys().cloned().collect::<Vec<_>>()
        };
        // Enforced and contained: captured authority and both containment
        // mechanisms are held; the trusted lane is not, even if node policy
        // accepts it.
        let contained = inspection(true, true).reported_guarantees(true, &native);
        for name in [
            ISOLATION_ENFORCED,
            FILESYSTEM_CAPTURED_EXECUTION,
            CALLBACK_CAPTURED_COMPATIBLE,
            "termination:scope",
            "recovery:scope",
            "termination:pid_namespace",
            "recovery:pid_namespace",
        ] {
            assert!(contained.provided.contains_key(name), "{name}");
        }
        assert!(
            contained.missing[SESSIONS_TRUSTED_PROCESS_GROUP].contains("enforced"),
            "{:?}",
            contained.missing
        );
        assert!(
            contained.provided[CALLBACK_CAPTURED_COMPATIBLE]
                .basis
                .contains("node-level")
        );
        // Not enforced, trusted sessions accepted: only the weaker grade.
        let trusted = inspection(true, false).reported_guarantees(false, &native);
        assert_eq!(provided(&trusted), [SESSIONS_TRUSTED_PROCESS_GROUP]);
        assert_eq!(
            trusted.provided[SESSIONS_TRUSTED_PROCESS_GROUP].source,
            lillux::process_control::ProofSource::Trusted
        );
        // Neither: nothing is held and every name says why.
        let bare = inspection(false, false).reported_guarantees(false, &native);
        assert!(bare.provided.is_empty(), "{:?}", bare.provided);
        assert!(bare.missing[SESSIONS_TRUSTED_PROCESS_GROUP].contains("does not accept"));
        // Enforced without a qualified mechanism holds no containment.
        let uncontained = inspection(false, false).reported_guarantees(true, &native);
        assert!(uncontained.provided.contains_key(ISOLATION_ENFORCED));
        assert!(!uncontained.provided.contains_key("termination:scope"));
        assert!(
            !uncontained
                .provided
                .contains_key("termination:pid_namespace")
        );
        // Under gVisor the cgroup scope and the accepted trusted lane are
        // provider-attested, as the capability report grades them; a PID
        // namespace proof keeps the grade its probe gave it.
        let emulated = GuaranteeHost {
            gvisor: true,
            scope_provider: "host_runtime".to_owned(),
            scope_absent: None,
        };
        let attested = inspection(false, true).reported_guarantees(true, &emulated);
        for name in ["termination:scope", "recovery:scope"] {
            let evidence = &attested.provided[name];
            assert_eq!(
                evidence.source,
                lillux::process_control::ProofSource::ProviderAttested
            );
            assert_eq!(evidence.provider, "host_runtime");
        }
        let attested_trusted = inspection(true, false).reported_guarantees(false, &emulated);
        assert_eq!(
            attested_trusted.provided[SESSIONS_TRUSTED_PROCESS_GROUP].source,
            lillux::process_control::ProofSource::ProviderAttested
        );
        // A bare requirement is a measured-only requirement, so it is not met.
        let bare_requirement =
            lillux::process_control::CapabilityRequirement::from("termination:scope");
        assert!(attested.meets(&bare_requirement).is_err());
    }

    #[test]
    fn pid_namespace_proof_is_judged_by_the_floor_as_the_report_grades_it() {
        use lillux::process_control::{
            CapabilityRequirement, CapabilityRequirementSpec, ProofSource,
        };
        let attested = || {
            PidNamespaceContainmentQualification::qualified_by(
                ProofSource::ProviderAttested,
                "emulated kernel",
            )
        };
        let measured =
            || PidNamespaceContainmentQualification::qualified_by(ProofSource::Measured, "native");
        let accepting = |grades: &[&str]| {
            CapabilityRequirement::Spec(CapabilityRequirementSpec {
                name: Some("termination:pid_namespace".to_owned()),
                any_of: Vec::new(),
                accept: grades.iter().map(|grade| (*grade).to_owned()).collect(),
                providers: Vec::new(),
            })
        };
        // A silent floor admits only a measured proof.
        assert!(measured().graded_by(&[]).qualified);
        let silent = attested().graded_by(&[]);
        assert!(
            !silent.qualified && silent.detail.contains("provider_attested"),
            "{silent:?}"
        );
        // A floor naming it by bare name accepts measured only, as the report grades.
        assert!(
            measured()
                .graded_by(&["termination:pid_namespace".into()])
                .qualified
        );
        assert!(
            !attested()
                .graded_by(&["termination:pid_namespace".into()])
                .qualified
        );
        // A floor that accepts the attested grade admits it; one that does
        // not, refuses it with the grade mismatch named.
        assert!(
            attested()
                .graded_by(&[accepting(&["provider_attested"])])
                .qualified
        );
        let refused = attested().graded_by(&[accepting(&["measured"])]);
        assert!(
            !refused.qualified && refused.detail.contains("provider_attested"),
            "{refused:?}"
        );
        // An unqualified probe stays unqualified whatever the floor says.
        let failed = PidNamespaceContainmentQualification::not_probed("refused")
            .graded_by(&[accepting(&["measured", "provider_attested"])]);
        assert!(!failed.qualified);
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
