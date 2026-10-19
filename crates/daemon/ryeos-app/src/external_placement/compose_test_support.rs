//! Installed-artifact fixtures for composing a binding without a bundle on
//! disk. They build the same registries a resolved bundle generation yields,
//! from real files and declared lifecycle coordinates, so the composer's
//! artifact reads run against genuine registry lookups.

use super::*;

/// Lifecycle generation that is only ever asked for its artifact coordinates.
/// Every operation that would contact a provider or run an adapter refuses.
#[derive(Debug)]
pub(crate) struct CoordinateOnlyBackend {
    backend_id: String,
    artifact_hash: String,
    settings_schema_digest: String,
    supervisor_hash: String,
    launcher_hash: String,
}

impl CoordinateOnlyBackend {
    pub(crate) fn shared(
        backend_id: &str,
        artifact_hash: &str,
        settings_schema_digest: &str,
        supervisor_hash: &str,
        launcher_hash: &str,
    ) -> Arc<dyn ExternalPlacementBackend> {
        Arc::new(Self {
            backend_id: backend_id.to_owned(),
            artifact_hash: artifact_hash.to_owned(),
            settings_schema_digest: settings_schema_digest.to_owned(),
            supervisor_hash: supervisor_hash.to_owned(),
            launcher_hash: launcher_hash.to_owned(),
        })
    }
}

impl ExternalPlacementBackend for CoordinateOnlyBackend {
    fn lifecycle_capabilities(&self) -> BTreeSet<LifecycleCapability> {
        BTreeSet::new()
    }

    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    fn artifact_hash(&self) -> &str {
        &self.artifact_hash
    }

    fn artifact_bytes(&self) -> u64 {
        4096
    }

    fn supervisor_artifact(&self) -> (&str, u64) {
        (&self.supervisor_hash, 8192)
    }

    fn launcher_artifact(&self) -> (&str, u64) {
        (&self.launcher_hash, 12288)
    }

    fn settings_schema_digest(&self) -> &str {
        &self.settings_schema_digest
    }

    fn qualify_offline(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
    ) -> Result<()> {
        bail!("coordinate-only backend qualifies nothing")
    }

    fn prepare_guest_package(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _reservation: &ExternalAllocationReservation,
        _occurrence: &ExternalAllocationOccurrence,
        _bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        _guest_inputs: &ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
        _activation_request_digest: &str,
        _parent: &lillux::PinnedDirectory,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<SupervisorGuestPackage> {
        bail!("coordinate-only backend prepares no guest package")
    }
}

/// Write `bytes` as an executable file at `path`.
pub(crate) fn write_executable(path: &Path, bytes: &[u8]) {
    lillux::atomic_write_with_mode(path, bytes, 0o755).unwrap();
}

/// Installed connector registry over the executables at `paths`.
pub(crate) fn connector_registry(
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<ExternalCandidateConnectorRegistry> {
    ExternalCandidateConnectorRegistry::from_paths(paths)
}

/// Content identity and size of the connector executable at `path`.
pub(crate) fn connector_coordinate(path: &Path) -> (String, u64) {
    let registry = ExternalCandidateConnectorRegistry::from_paths([path.to_path_buf()]).unwrap();
    let (coordinate, _) = registry.artifacts.iter().next().unwrap();
    coordinate.clone()
}

/// Installed provider configuration adapter for `declaration`, pinned to the
/// executable at `adapter`.
pub(crate) fn provider_configuration(
    declaration: ryeos_external_execution_contract::ExternalProviderDeclaration,
    adapter: &Path,
) -> Result<InstalledExternalProviderConfiguration> {
    declaration.validate()?;
    let pinned = lillux::secure_fs::open_pinned_regular_file_no_follow(adapter)?;
    pinned.require_executable()?;
    let observation = pinned.observation()?;
    let artifact_bytes = observation.size();
    let artifact_hash = pinned.digest_stable_exact(&observation)?;
    Ok(InstalledExternalProviderConfiguration {
        declaration,
        bundle_manifest_digest: "0".repeat(64),
        signer_fingerprint: "fixture-signer".to_owned(),
        artifact_hash,
        artifact_bytes,
        executable: pinned.inherited_descriptor_authority()?,
    })
}

/// Content identity and size of the provider configuration adapter at `path`.
pub(crate) fn provider_configuration_coordinate(path: &Path) -> (String, u64) {
    let pinned = lillux::secure_fs::open_pinned_regular_file_no_follow(path).unwrap();
    let observation = pinned.observation().unwrap();
    (
        pinned.digest_stable_exact(&observation).unwrap(),
        observation.size(),
    )
}
