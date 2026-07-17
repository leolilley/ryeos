use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use ryeos_isolation_protocol::{
    InspectedArtifact, IsolationArtifactRole, IsolationBackendSelection, IsolationCapability,
};
use serde::{Deserialize, Serialize};

use super::{
    IsolationEnvironmentPolicy, IsolationFilesystemPolicy, IsolationLimitsPolicy, IsolationMode,
    IsolationNetworkPolicy,
};

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
