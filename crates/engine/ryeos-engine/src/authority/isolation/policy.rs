use ryeos_isolation_protocol::{FixedParentViewLimits, IsolationBackendSelection};
use serde::{Deserialize, Serialize};

pub const ISOLATION_POLICY_VERSION: u32 = 7;
#[cfg(any(test, feature = "test-support"))]
pub const TEST_ISOLATION_POLICY_RELATIVE_PATH: &str = "test-fixtures/isolation-policy.yaml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationPolicy {
    pub version: u32,
    pub mode: IsolationMode,
    pub backend: Option<IsolationBackendSelection>,
    pub process_scopes: IsolationProcessScopePolicy,
    /// Explicit opt-in for trusted dedicated workers that use RyeOS's direct
    /// process-group lifecycle without claiming qualified containment.
    pub trusted_process_group_sessions: bool,
    pub filesystem: IsolationFilesystemPolicy,
    pub network: IsolationNetworkPolicy,
    pub environment: IsolationEnvironmentPolicy,
    pub limits: IsolationLimitsPolicy,
}

impl IsolationPolicy {
    /// Explicit policy-free authoring/test boundary. Running nodes compile
    /// their mandatory signed isolation policy instead of using this value.
    pub fn disabled_for_authoring() -> Self {
        Self {
            version: ISOLATION_POLICY_VERSION,
            mode: IsolationMode::Disabled,
            backend: None,
            process_scopes: IsolationProcessScopePolicy::Unconfigured {},
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
        /// Node permission, not evidence that a particular launch has a scope.
        /// Requires the explicit pid_namespace_nested proc ceiling and a
        /// qualified adapter capability. Ordinary unscoped launches still
        /// receive only read-only task-only proc, not this broader surface.
        nested_sandbox: bool,
    },
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
