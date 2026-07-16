use ryeos_isolation_protocol::IsolationBackendSelection;
use serde::{Deserialize, Serialize};

pub const ISOLATION_POLICY_VERSION: u32 = 1;
pub const ISOLATION_POLICY_RELATIVE_PATH: &str = "node/isolation.yaml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationPolicy {
    pub version: u32,
    pub mode: IsolationMode,
    pub backend: IsolationBackendSelection,
    pub filesystem: IsolationFilesystemPolicy,
    pub network: IsolationNetworkPolicy,
    pub environment: IsolationEnvironmentPolicy,
    pub limits: IsolationLimitsPolicy,
}

impl IsolationPolicy {
    pub fn default_disabled() -> Self {
        Self {
            version: ISOLATION_POLICY_VERSION,
            mode: IsolationMode::Disabled,
            backend: IsolationBackendSelection {
                bundle: "sandbox-linux-bubblewrap".to_string(),
                implementation: "linux-bubblewrap".to_string(),
            },
            filesystem: IsolationFilesystemPolicy {
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    Disabled,
    Enforce,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationFilesystemPolicy {
    pub readable: Vec<String>,
    pub writable: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationNetworkPolicy {
    pub mode: IsolationNetworkMode,
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
