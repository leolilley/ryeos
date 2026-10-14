use ryeos_isolation_protocol::{FixedParentViewLimits, IsolationBackendSelection};
use serde::{Deserialize, Serialize};

pub const ISOLATION_POLICY_VERSION: u32 = 9;
/// Required outright, together with the nested proc ceiling, this host
/// capability is the node's permission to grant a nested sandbox.
pub use lillux::process_control::ISOLATION_NAMESPACES;
#[cfg(any(test, feature = "test-support"))]
pub const TEST_ISOLATION_POLICY_RELATIVE_PATH: &str = "test-fixtures/isolation-policy.yaml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationPolicy {
    pub version: u32,
    pub mode: IsolationMode,
    pub backend: Option<IsolationBackendSelection>,
    pub process_scopes: IsolationProcessScopePolicy,
    /// Capabilities this node must prove before it starts, sorted and
    /// unique. Each entry is a name, or an `any_of` set of names any one of
    /// which meets it, so a weaker grade is accepted only where the floor
    /// names it. Names are open strings matched against the node capability
    /// report; a name this build cannot prove is refused, never ignored.
    pub requires_capabilities: Vec<lillux::process_control::CapabilityRequirement>,
    /// Executable names of inits trusted to exit when their only child
    /// exits, sorted and unique. When the node's parent is pid 1 and pid 1
    /// is one of these, the node's exit is trusted to end its container
    /// enclosure; the host proves that only when the node is pid 1 itself.
    pub enclosure_inits: Vec<String>,
    /// Explicit opt-in for trusted dedicated workers that use RyeOS's direct
    /// process-group lifecycle without claiming qualified containment.
    pub trusted_process_group_sessions: bool,
    pub filesystem: IsolationFilesystemPolicy,
    pub network: IsolationNetworkPolicy,
    pub environment: IsolationEnvironmentPolicy,
    pub limits: IsolationLimitsPolicy,
}

impl IsolationPolicy {
    /// Per-scope guarantees this policy's capability floor requires outright:
    /// the set the scope provider is asked to qualify, and nothing more. A
    /// set of alternatives never names one, which validation enforces.
    pub fn required_process_scope_capabilities(
        &self,
    ) -> std::collections::BTreeSet<lillux::ProcessScopeCapability> {
        self.requires_capabilities
            .iter()
            .filter_map(|entry| {
                let [name] = entry.names() else {
                    return None;
                };
                lillux::ProcessScopeCapability::from_capability_name(name)
            })
            .collect()
    }

    /// Node permission to grant scoped launches a nested sandbox, derived
    /// from the floor requiring `isolation:namespaces` outright together
    /// with the `pid_namespace_nested` proc ceiling. Validation keeps the
    /// two in step, so neither alone ever means it. This is permission, not
    /// evidence that a particular launch has a scope.
    pub fn nested_sandbox(&self) -> bool {
        nested_sandbox_from(
            &self.requires_capabilities,
            self.filesystem.proc_filesystem,
        )
    }

    /// Explicit policy-free authoring/test boundary. Running nodes compile
    /// their mandatory signed isolation policy instead of using this value.
    pub fn disabled_for_authoring() -> Self {
        Self {
            version: ISOLATION_POLICY_VERSION,
            mode: IsolationMode::Disabled,
            backend: None,
            process_scopes: IsolationProcessScopePolicy::Unconfigured {},
            requires_capabilities: Vec::new(),
            enclosure_inits: Vec::new(),
            trusted_process_group_sessions: false,
            filesystem: IsolationFilesystemPolicy {
                proc_filesystem: ryeos_isolation_protocol::IsolationProcFilesystem::Empty,
                // Values for this explicit non-enforcing authoring fixture
                // only. Never call this constructor to fill missing node
                // policy: enforced launch limits come from its signed member.
                live_project: IsolationLiveProjectPolicy::FixedParents {
                    limits: FixedParentViewLimits {
                        max_entries: 2048,
                        max_depth: 64,
                    },
                },
                readable: vec![
                    "{node_public_identity}".to_string(),
                    "{daemon_socket}".to_string(),
                    "{bundle_roots}".to_string(),
                    "{node_trusted_keys}".to_string(),
                    "{verified_code}".to_string(),
                ],
                writable: vec!["{project}".to_string(), "{checkpoint_dir}".to_string()],
            },
            network: IsolationNetworkPolicy {
                mode: IsolationNetworkMode::Host,
                runtime_files: Vec::new(),
            },
            environment: IsolationEnvironmentPolicy {
                allow: vec!["*".to_string()],
            },
            limits: IsolationLimitsPolicy {
                open_files: Some(1024),
                stdout_bytes: 8_388_608,
                stderr_bytes: 8_388_608,
                verified_artifact_file_bytes: 67_108_864,
                verified_artifact_total_bytes: 268_435_456,
                verified_artifact_files: 4_096,
            },
        }
    }
}

/// Node semantic authority for the optional host facility, not permission to
/// downgrade an execution which requires it. Unconfigured nodes must refuse
/// scope-backed launch; ordinary shared-group launches retain their
/// no-group-escape contract. Native provider selection, delegation paths, and
/// scope configuration are deliberately absent: Lillux retains and opens those
/// only from the protected host association at daemon boot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum IsolationProcessScopePolicy {
    // Keep this an empty struct variant: serde's internally-tagged unit
    // variant accepts unknown fields even with deny_unknown_fields.
    Unconfigured {},
    Required {
        control_timeout_ms: u64,
    },
}

/// Whether `floor` requires `name` outright: as the only name of an entry,
/// never as one alternative in a set.
pub fn floor_requires_outright(
    floor: &[lillux::process_control::CapabilityRequirement],
    name: &str,
) -> bool {
    floor
        .iter()
        .any(|entry| matches!(entry.names(), [only] if only == name))
}

/// Nested-sandbox permission as [`IsolationPolicy::nested_sandbox`] derives
/// it, for callers holding the floor and proc ceiling apart from the policy.
pub fn nested_sandbox_from(
    floor: &[lillux::process_control::CapabilityRequirement],
    proc_filesystem: ryeos_isolation_protocol::IsolationProcFilesystem,
) -> bool {
    floor_requires_outright(floor, ISOLATION_NAMESPACES)
        && proc_filesystem == ryeos_isolation_protocol::IsolationProcFilesystem::PidNamespaceNested
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    Disabled,
    Enforce,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationFilesystemPolicy {
    pub proc_filesystem: ryeos_isolation_protocol::IsolationProcFilesystem,
    pub live_project: IsolationLiveProjectPolicy,
    pub readable: Vec<String>,
    pub writable: Vec<String>,
}

/// Explicit node-owned confined-live contract, never selected by a workload.
/// Fixed parent entries are an admitted restriction, not silent CoW behavior.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum IsolationLiveProjectPolicy {
    FixedParents { limits: FixedParentViewLimits },
}

impl IsolationLiveProjectPolicy {
    pub fn limits(self) -> FixedParentViewLimits {
        match self {
            Self::FixedParents { limits } => limits,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationNetworkPolicy {
    pub mode: IsolationNetworkMode,
    /// Explicit target-local transport inputs, independent of general host
    /// filesystem access. Empty means none; no implicit resolver/trust fallback.
    pub runtime_files: Vec<IsolationNetworkRuntimeFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationNetworkRuntimeFile {
    pub source: std::path::PathBuf,
    pub destination: std::path::PathBuf,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IsolationNetworkMode {
    Host,
    Isolated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationEnvironmentPolicy {
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationLimitsPolicy {
    pub open_files: Option<u64>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub verified_artifact_file_bytes: u64,
    pub verified_artifact_total_bytes: u64,
    pub verified_artifact_files: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::process_control::{CapabilityRequirement, CapabilityRequirementSpec};

    fn outright(name: &str, providers: &[&str]) -> CapabilityRequirement {
        CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: Some(name.to_owned()),
            any_of: Vec::new(),
            accept: Vec::new(),
            providers: providers.iter().map(|id| (*id).to_owned()).collect(),
        })
    }

    fn any_of(names: &[&str]) -> CapabilityRequirement {
        CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: None,
            any_of: names.iter().map(|name| (*name).to_owned()).collect(),
            accept: Vec::new(),
            providers: Vec::new(),
        })
    }

    #[test]
    fn per_scope_requirements_count_only_when_required_outright() {
        let mut policy = IsolationPolicy::disabled_for_authoring();
        policy.requires_capabilities = vec![
            any_of(&["accounting", "residency:swap_absent"]),
            outright("quiescence:scope", &["host_service"]),
            "recovery:scope".into(),
            "termination:scope".into(),
        ];
        assert_eq!(
            policy.required_process_scope_capabilities(),
            std::collections::BTreeSet::from([
                lillux::ProcessScopeCapability::Quiescence,
                lillux::ProcessScopeCapability::Termination,
                lillux::ProcessScopeCapability::Recovery,
            ])
        );
        policy.requires_capabilities = vec![any_of(&["recovery:scope", "termination:scope"])];
        assert!(policy.required_process_scope_capabilities().is_empty());
    }

    #[test]
    fn nested_sandbox_needs_both_the_floor_entry_and_the_nested_proc() {
        let mut policy = IsolationPolicy::disabled_for_authoring();
        assert!(!policy.nested_sandbox());
        policy.requires_capabilities = vec![ISOLATION_NAMESPACES.into()];
        assert!(!policy.nested_sandbox());
        policy.filesystem.proc_filesystem =
            ryeos_isolation_protocol::IsolationProcFilesystem::PidNamespaceNested;
        assert!(policy.nested_sandbox());
        policy.requires_capabilities = vec![outright(ISOLATION_NAMESPACES, &["host"])];
        assert!(policy.nested_sandbox());
        policy.requires_capabilities = vec![any_of(&[ISOLATION_NAMESPACES, "time"])];
        assert!(!policy.nested_sandbox());
        policy.requires_capabilities = Vec::new();
        assert!(!policy.nested_sandbox());
        assert!(!floor_requires_outright(&policy.requires_capabilities, ISOLATION_NAMESPACES));
    }

    #[test]
    fn the_removed_nested_sandbox_key_is_refused() {
        let error = serde_json::from_value::<IsolationProcessScopePolicy>(serde_json::json!({
            "mode": "required", "control_timeout_ms": 1000, "nested_sandbox": true,
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("nested_sandbox"), "{error}");
        let required: IsolationProcessScopePolicy = serde_json::from_value(serde_json::json!({
            "mode": "required", "control_timeout_ms": 1000,
        }))
        .unwrap();
        assert_eq!(
            required,
            IsolationProcessScopePolicy::Required {
                control_timeout_ms: 1000
            }
        );
    }
}
