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
    relay_scope_setting: Option<String>,
    capabilities: BTreeSet<LifecycleCapability>,
    /// Whether the adapter admits a lease. A refusing one says it admits none.
    admits_leases: bool,
    /// When set, `admit_lease` runs the shared provider-neutral judge over
    /// these facts instead of answering `admits_leases`.
    lease_spec: Option<DerivedLeaseSpec>,
}

/// One operation kind of a provider spec, as far as lease guarantees are
/// concerned.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SpecOperation {
    /// Creates the lease bound to its reservation, taking the provider
    /// timeout from the reserved lifetime and the guest network from the
    /// lease's allow list.
    CreateOnceBoundToReservation,
    /// Observes the exact terminal state of an occurrence.
    ObserveExactOccurrence,
}

/// Provider facts whose attested guarantees are derived from the spec's
/// operation kinds alone, the way an adapter derives them: a guarantee is
/// never named directly.
#[derive(Debug)]
pub(crate) struct DerivedLeaseSpec {
    operations: Vec<SpecOperation>,
    network_policy: ryeos_remote_lease_core::NetworkPolicyCapability,
}

impl DerivedLeaseSpec {
    pub(crate) fn new(operations: Vec<SpecOperation>) -> Self {
        use ryeos_remote_lease_core::*;
        Self {
            operations,
            network_policy: NetworkPolicyCapability {
                creatable_types: vec![NetworkPolicyType::DenyAll, NetworkPolicyType::AllowList],
                allow_list: Some(AllowListCapability {
                    rule_protocol: EgressProtocol::Https,
                    max_rules: 4,
                    host_syntax: EgressHostSyntax::ExactDnsName,
                }),
            },
        }
    }

    /// The guarantees these operation kinds derive.
    pub(crate) fn derived_guarantees(&self) -> BTreeSet<&'static str> {
        use ryeos_remote_lease_core::*;
        let mut guarantees = BTreeSet::new();
        for operation in &self.operations {
            match operation {
                SpecOperation::CreateOnceBoundToReservation => {
                    guarantees.extend([
                        LEASE_CREATE_BOUND,
                        LEASE_LIFETIME_BOUND,
                        LEASE_NETWORK_POLICY,
                    ]);
                }
                SpecOperation::ObserveExactOccurrence => {
                    guarantees.insert(LEASE_TERMINATE_OBSERVED);
                }
            }
        }
        guarantees
    }
}

impl ryeos_remote_lease_core::LeaseProviderFacts for DerivedLeaseSpec {
    fn evidence_provider(&self) -> &str {
        "derived_fixture_provider_v1"
    }

    fn maximum_lease_seconds(&self) -> u32 {
        24 * 60 * 60
    }

    fn budget_weight(&self) -> u32 {
        1
    }

    fn attested_lease_guarantees(&self) -> BTreeSet<&'static str> {
        self.derived_guarantees()
    }

    fn guest_channel_profile(&self) -> Result<ryeos_remote_lease_core::GuestChannelProfile> {
        Ok(ryeos_remote_lease_core::GuestChannelProfile::ControllerRelayedFrames)
    }

    fn guest_activation_declared(&self) -> bool {
        true
    }

    fn network_policy_capability(&self) -> &ryeos_remote_lease_core::NetworkPolicyCapability {
        &self.network_policy
    }

    fn egress_evidence_basis(&self, policy: &ryeos_remote_lease_core::EgressPolicy) -> String {
        format!("fixture provider creates exactly [{}]", policy.rules_text())
    }

    fn valid_lease_id(&self, id: &str) -> bool {
        id.starts_with("lease-")
    }
}

impl CoordinateOnlyBackend {
    pub(crate) fn shared(
        backend_id: &str,
        artifact_hash: &str,
        settings_schema_digest: &str,
        supervisor_hash: &str,
        launcher_hash: &str,
    ) -> Arc<dyn ExternalPlacementBackend> {
        Self::shared_with(
            backend_id,
            artifact_hash,
            settings_schema_digest,
            supervisor_hash,
            launcher_hash,
            BTreeSet::from([
                LifecycleCapability::SupervisorActivation,
                LifecycleCapability::ExactTerminalObservation,
            ]),
            false,
        )
    }

    /// A generation with exactly these capabilities, which does or does not
    /// admit leases.
    pub(crate) fn shared_with(
        backend_id: &str,
        artifact_hash: &str,
        settings_schema_digest: &str,
        supervisor_hash: &str,
        launcher_hash: &str,
        capabilities: BTreeSet<LifecycleCapability>,
        admits_leases: bool,
    ) -> Arc<dyn ExternalPlacementBackend> {
        Arc::new(Self {
            backend_id: backend_id.to_owned(),
            artifact_hash: artifact_hash.to_owned(),
            settings_schema_digest: settings_schema_digest.to_owned(),
            supervisor_hash: supervisor_hash.to_owned(),
            launcher_hash: launcher_hash.to_owned(),
            relay_scope_setting: Some("owner_id".to_owned()),
            capabilities,
            admits_leases,
            lease_spec: None,
        })
    }

    /// A generation whose lease admission is the shared judge over `spec`'s
    /// derived guarantees.
    pub(crate) fn shared_with_lease_spec(
        backend_id: &str,
        artifact_hash: &str,
        settings_schema_digest: &str,
        supervisor_hash: &str,
        launcher_hash: &str,
        capabilities: BTreeSet<LifecycleCapability>,
        spec: DerivedLeaseSpec,
    ) -> Arc<dyn ExternalPlacementBackend> {
        Arc::new(Self {
            backend_id: backend_id.to_owned(),
            artifact_hash: artifact_hash.to_owned(),
            settings_schema_digest: settings_schema_digest.to_owned(),
            supervisor_hash: supervisor_hash.to_owned(),
            launcher_hash: launcher_hash.to_owned(),
            relay_scope_setting: Some("owner_id".to_owned()),
            capabilities,
            admits_leases: false,
            lease_spec: Some(spec),
        })
    }
}

impl ExternalPlacementBackend for CoordinateOnlyBackend {
    fn lifecycle_capabilities(&self) -> BTreeSet<LifecycleCapability> {
        self.capabilities.clone()
    }

    fn admit_lease(
        &self,
        inputs: &crate::external_lifecycle_adapter::lease_admission::LeaseAdmissionInputs,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<crate::external_lifecycle_adapter::lease_admission::AdmittedLease> {
        use crate::external_lifecycle_adapter::lease_admission as admission;
        if let Some(spec) = &self.lease_spec {
            let adapter_id = "remote-lease-fixture";
            let spec_digest = lillux::sha256_hex(b"derived lease fixture spec");
            let request = admission::canonical_request(adapter_id, &spec_digest, inputs)?;
            let parsed: ryeos_remote_lease_core::LeaseAdmissionRequest =
                ryeos_external_execution_contract::from_json_slice_strict(
                    &request,
                    ryeos_external_execution_contract::MAX_LEASE_ADMISSION_BYTES,
                )?;
            let verdict = ryeos_external_execution_contract::canonical_json(
                &ryeos_remote_lease_core::admit_before_spend(
                    spec,
                    &parsed,
                    &lillux::sha256_hex(&request),
                ),
            )?;
            let budget_weight =
                admission::require_admitted(&verdict, &request, &spec_digest, adapter_id)?;
            return Ok(admission::AdmittedLease {
                request,
                budget_weight,
            });
        }
        ensure!(
            self.admits_leases,
            "lease admission refused: external placement backend `{}` admits no leases",
            self.backend_id()
        );
        Ok(
            crate::external_lifecycle_adapter::lease_admission::AdmittedLease {
                request: b"{\"admitted\":true}".to_vec(),
                budget_weight: 1,
            },
        )
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

    fn relay_scope_setting(&self) -> Option<&str> {
        self.relay_scope_setting.as_deref()
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
