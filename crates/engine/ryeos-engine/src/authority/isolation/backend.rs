use std::collections::{BTreeMap, BTreeSet};

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
    pub adapter_handle: lillux::InheritedDescriptorAuthority,
    pub artifact_handles: BTreeMap<IsolationArtifactRole, lillux::InheritedDescriptorAuthority>,
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

/// Actual verified executable captures corresponding to this same backend.
/// Supplied-trust content facts only; a protected App consumer must separately
/// admit node trust, registered generation, signed declaration and launch.
/// Public backend metadata cannot manufacture the captured executable values.
#[derive(Debug)]
pub struct VerifiedIsolationBackendCapture {
    backend: std::sync::Arc<ResolvedIsolationBackend>,
    adapter: crate::binary_resolver::VerifiedCapturedExecutable,
    artifacts: BTreeMap<IsolationArtifactRole, crate::binary_resolver::VerifiedCapturedExecutable>,
}

impl VerifiedIsolationBackendCapture {
    /// Associate actual captures with their same retained backend descriptors.
    /// This validates correspondence, not node admission or native authority.
    pub fn from_captures(
        backend: std::sync::Arc<ResolvedIsolationBackend>,
        adapter: crate::binary_resolver::VerifiedCapturedExecutable,
        artifacts: BTreeMap<
            IsolationArtifactRole,
            crate::binary_resolver::VerifiedCapturedExecutable,
        >,
    ) -> Result<Self, EngineError> {
        let captured = Self {
            backend,
            adapter,
            artifacts,
        };
        captured.validate_correspondence()?;
        Ok(captured)
    }

    pub fn backend(&self) -> &std::sync::Arc<ResolvedIsolationBackend> {
        &self.backend
    }

    pub fn adapter(&self) -> &crate::binary_resolver::VerifiedCapturedExecutable {
        &self.adapter
    }

    pub fn artifacts(
        &self,
    ) -> &BTreeMap<IsolationArtifactRole, crate::binary_resolver::VerifiedCapturedExecutable> {
        &self.artifacts
    }

    /// Require the same retained backend object, not a recreated metadata DTO.
    pub fn require_same_backend(
        &self,
        backend: &std::sync::Arc<ResolvedIsolationBackend>,
    ) -> Result<(), EngineError> {
        if !std::sync::Arc::ptr_eq(&self.backend, backend) {
            return Err(refused(
                "verified captures belong to a different retained backend".into(),
            ));
        }
        self.validate_correspondence()
    }

    fn validate_correspondence(&self) -> Result<(), EngineError> {
        self.backend.validate()?;
        let adapter = self.adapter.captured();
        if adapter.identity.signer_fingerprint != self.backend.signer_fingerprint
            || adapter.identity.content_hash != self.backend.adapter_digest
            || adapter.source_proof.selected_item_ref
                != format!(
                    "bin/{}/{}",
                    adapter.identity.target_triple, self.backend.declaration.adapter
                )
            || adapter.identity.target_triple != env!("RYEOS_ENGINE_HOST_TRIPLE")
            || !adapter
                .handle
                .same_file_identity(&self.backend.adapter_handle)
                .map_err(|error| refused(format!("compare retained adapter capture: {error}")))?
        {
            return Err(refused(
                "verified adapter capture differs from retained backend".into(),
            ));
        }
        let roles = self.artifacts.keys().copied().collect::<BTreeSet<_>>();
        if roles
            != self
                .backend
                .declaration
                .artifacts
                .keys()
                .copied()
                .collect::<BTreeSet<_>>()
        {
            return Err(refused(
                "verified artifact capture roles differ from retained backend".into(),
            ));
        }
        for (role, captured) in &self.artifacts {
            let artifact = captured.captured();
            let expected = &self.backend.declaration.artifacts[role];
            if artifact.identity.signer_fingerprint != self.backend.signer_fingerprint
                || artifact.identity.target_triple != adapter.identity.target_triple
                || artifact.source_proof.selected_item_ref
                    != format!("bin/{}/{expected}", artifact.identity.target_triple)
                || artifact.identity.content_hash != self.backend.inspected_artifacts[role].digest
                || !artifact
                    .handle
                    .same_file_identity(&self.backend.artifact_handles[role])
                    .map_err(|error| {
                        refused(format!("compare retained artifact capture: {error}"))
                    })?
            {
                return Err(refused(
                    "verified artifact capture differs from retained backend".into(),
                ));
            }
        }
        // BundleManifest YAML body digest and executor manifest hashes are
        // distinct identities. Both are retained; no equality is inferred.
        Ok(())
    }
}
