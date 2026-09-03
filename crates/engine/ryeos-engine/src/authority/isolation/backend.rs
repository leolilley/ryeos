use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ryeos_isolation_protocol::{
    InspectedArtifact, IsolationArtifactRole, IsolationBackendDeclaration,
    IsolationBackendSelection, IsolationCapability,
};

use super::refused;
use crate::error::EngineError;

#[derive(Debug, Clone)]
pub struct ResolvedIsolationBackend {
    pub selection: IsolationBackendSelection,
    pub declaration: IsolationBackendDeclaration,
    pub bundle_manifest_digest: String,
    pub signer_fingerprint: String,
    pub adapter_digest: String,
    pub adapter_handle: Arc<std::fs::File>,
    pub artifact_handles: BTreeMap<IsolationArtifactRole, Arc<std::fs::File>>,
    pub adapter_build: String,
    pub effective_capabilities: BTreeSet<IsolationCapability>,
    pub inspected_artifacts: BTreeMap<IsolationArtifactRole, InspectedArtifact>,
}

impl ResolvedIsolationBackend {
    pub fn validate(&self) -> Result<(), EngineError> {
        self.selection
            .validate()
            .map_err(|error| refused(error.to_string()))?;
        self.declaration
            .validate()
            .map_err(|error| refused(error.to_string()))?;
        if self.declaration.id != self.selection.implementation {
            return Err(refused(
                "resolved isolation implementation does not match node policy".to_string(),
            ));
        }
        if self.signer_fingerprint.is_empty()
            || self.bundle_manifest_digest.is_empty()
            || self.adapter_digest.len() != 64
            || !self
                .adapter_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(refused(
                "resolved isolation backend lacks signed bundle identity".to_string(),
            ));
        }
        let declared_roles = self
            .declaration
            .artifacts
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        let captured_roles = self
            .artifact_handles
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        let inspected_roles = self
            .inspected_artifacts
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        if captured_roles != declared_roles || inspected_roles != declared_roles {
            return Err(refused(
                "resolved isolation backend artifact sets do not exactly match its signed declaration"
                    .to_string(),
            ));
        }
        for artifact in self.inspected_artifacts.values() {
            artifact.validate().map_err(|error| {
                refused(format!("invalid inspected isolation artifact: {error}"))
            })?;
        }
        if !self
            .effective_capabilities
            .is_subset(&self.declaration.capabilities)
        {
            return Err(refused(
                "resolved isolation adapter capabilities exceed its signed declaration".to_string(),
            ));
        }
        Ok(())
    }
}
