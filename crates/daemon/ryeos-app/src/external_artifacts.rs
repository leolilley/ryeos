//! Exact signed-bundle composition for external execution artifacts.
//!
//! This module is the only startup path from bundle declarations to executable
//! authority. It runs inside the engine's checked bundle-generation read and
//! never searches PATH, daemon siblings, project content, or unsigned source
//! manifests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use ryeos_engine::binary_resolver::{CapturedExecutable, capture_bundle_binary_ref};
use ryeos_external_execution_contract::{
    ExternalLifecycleAdapterDeclaration, LifecycleProviderSpecIdentity,
    MAX_LIFECYCLE_PROVIDER_SPEC_BYTES,
};

use crate::external_placement::{
    ExternalCandidateConnectorRegistry, ExternalPlacementBackend, ExternalPlacementBackendRegistry,
    ExternalProviderConfigurationRegistry, InstalledExternalProviderConfiguration,
};

#[derive(Debug)]
pub struct ResolvedExternalLifecycleArtifacts {
    pub declaration: ExternalLifecycleAdapterDeclaration,
    pub bundle_manifest_digest: String,
    pub signer_fingerprint: String,
    pub adapter: CapturedExecutable,
    pub supervisor: CapturedExecutable,
    pub launcher: CapturedExecutable,
    pub restoration_verifier: Option<CapturedExecutable>,
    pub provider_spec: CapturedLifecycleProviderSpec,
    pub snapshot_production_spec: Option<CapturedLifecycleProviderSpec>,
}

#[derive(Debug)]
pub struct CapturedLifecycleProviderSpec {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub authority: lillux::InheritedDescriptorAuthority,
}

#[derive(Debug)]
pub struct ResolvedExternalExecutionArtifacts {
    pub connectors: ExternalCandidateConnectorRegistry,
    pub provider_configurations: ExternalProviderConfigurationRegistry,
    pub placement_backends: ExternalPlacementBackendRegistry,
}

pub fn resolve_external_execution_artifacts(
    bundle_roots: &[PathBuf],
    node_trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<ResolvedExternalExecutionArtifacts> {
    let target = lillux::platform::current_binary_target()?;
    let mut connectors = Vec::new();
    let mut configurations = Vec::new();
    let mut lifecycle_adapters = Vec::new();
    let mut provider_owners = BTreeMap::new();
    let mut lifecycle_owners = BTreeMap::new();

    for root in bundle_roots {
        let name = root
            .file_name()
            .and_then(|value| value.to_str())
            .context("registered bundle root has no UTF-8 name")?;
        let verified = ryeos_bundle::manifest::load_verified_manifest(
            &root.join(ryeos_engine::AI_DIR),
            name,
            node_trust_store,
        )
        .with_context(|| format!("verify external execution bundle `{name}`"))?;

        for declaration in &verified.manifest.external_providers {
            declaration.validate()?;
            if !declaration
                .targets
                .iter()
                .any(|candidate| candidate == target)
            {
                continue;
            }
            ensure!(
                provider_owners
                    .insert(declaration.id.clone(), name.to_owned())
                    .is_none(),
                "external provider declaration `{}` has more than one signed bundle owner",
                declaration.id
            );
            let connector = capture_declared_executable(
                root,
                node_trust_store,
                &verified.signer_fingerprint,
                &declaration.connector,
                "external provider connector",
            )?;
            let configuration_adapter = capture_declared_executable(
                root,
                node_trust_store,
                &verified.signer_fingerprint,
                &declaration.configuration_adapter,
                "external provider configuration adapter",
            )?;
            connectors.push(connector);
            configurations.push(InstalledExternalProviderConfiguration::new(
                declaration.clone(),
                verified.body_digest.clone(),
                verified.signer_fingerprint.clone(),
                configuration_adapter,
            )?);
        }

        for declaration in &verified.manifest.external_lifecycle_adapters {
            declaration.validate()?;
            if !declaration
                .targets
                .iter()
                .any(|candidate| candidate == target)
            {
                continue;
            }
            ensure!(
                lifecycle_owners
                    .insert(declaration.id.clone(), name.to_owned())
                    .is_none(),
                "external lifecycle adapter `{}` has more than one signed bundle owner",
                declaration.id
            );
            lifecycle_adapters.push(ResolvedExternalLifecycleArtifacts {
                declaration: declaration.clone(),
                bundle_manifest_digest: verified.body_digest.clone(),
                signer_fingerprint: verified.signer_fingerprint.clone(),
                adapter: capture_declared_executable(
                    root,
                    node_trust_store,
                    &verified.signer_fingerprint,
                    &declaration.adapter,
                    "external lifecycle adapter",
                )?,
                supervisor: capture_declared_executable(
                    root,
                    node_trust_store,
                    &verified.signer_fingerprint,
                    &declaration.supervisor,
                    "external candidate supervisor",
                )?,
                launcher: capture_declared_executable(
                    root,
                    node_trust_store,
                    &verified.signer_fingerprint,
                    &declaration.launcher,
                    "external candidate launcher",
                )?,
                restoration_verifier: declaration
                    .restoration_verifier
                    .as_ref()
                    .map(|name| {
                        capture_declared_executable(
                            root,
                            node_trust_store,
                            &verified.signer_fingerprint,
                            name,
                            "external restoration verifier",
                        )
                    })
                    .transpose()?,
                provider_spec: capture_declared_provider_spec(root, &declaration.provider_spec)?,
                snapshot_production_spec: declaration
                    .snapshot_production_spec
                    .as_ref()
                    .map(|identity| capture_declared_provider_spec(root, identity))
                    .transpose()?,
            });
        }
    }

    let backends = lifecycle_adapters
        .into_iter()
        .map(|artifacts| {
            Ok(Arc::new(
                crate::external_lifecycle_adapter::ExecutableExternalPlacementBackend::new(
                    artifacts,
                )?,
            ) as Arc<dyn ExternalPlacementBackend>)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ResolvedExternalExecutionArtifacts {
        connectors: ExternalCandidateConnectorRegistry::from_captured(connectors)?,
        provider_configurations: ExternalProviderConfigurationRegistry::from_artifacts(
            configurations,
        )?,
        placement_backends: ExternalPlacementBackendRegistry::from_backends(backends)?,
    })
}

fn capture_declared_provider_spec(
    root: &std::path::Path,
    identity: &LifecycleProviderSpecIdentity,
) -> Result<CapturedLifecycleProviderSpec> {
    // `validate` restricts this to a normalized bundle-relative path, and
    // Lillux opens each component without following symlinks.
    let path = root.join(&identity.path);
    let pinned = lillux::secure_fs::open_pinned_regular_file_no_follow(&path)
        .with_context(|| format!("open signed lifecycle provider spec `{}`", identity.path))?;
    let observation = pinned.observation()?;
    let bytes = observation.size();
    ensure!(
        (1..=MAX_LIFECYCLE_PROVIDER_SPEC_BYTES).contains(&bytes),
        "signed lifecycle provider spec exceeds its byte bound"
    );
    ensure!(
        pinned.permission_mode()? & 0o111 == 0,
        "signed lifecycle provider spec must not be executable"
    );
    let captured =
        pinned.capture_sealed_bounded(&observation, MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?;
    ensure!(
        captured.digest() == identity.sha256,
        "signed lifecycle provider spec digest does not match its declaration"
    );
    Ok(CapturedLifecycleProviderSpec {
        path: identity.path.clone(),
        sha256: identity.sha256.clone(),
        bytes,
        authority: captured.authority().clone(),
    })
}

fn capture_declared_executable(
    root: &std::path::Path,
    trust: &ryeos_engine::trust::TrustStore,
    bundle_signer: &str,
    executable: &str,
    role: &str,
) -> Result<CapturedExecutable> {
    let captured = capture_bundle_binary_ref(&format!("bin:{executable}"), root, trust)
        .with_context(|| format!("capture signed {role} `{executable}`"))?;
    ensure!(
        captured.identity.signer_fingerprint == bundle_signer,
        "{role} signer does not match its declaring bundle"
    );
    Ok(captured)
}
