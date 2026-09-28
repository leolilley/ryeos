//! Closed executable runner for signed external occurrence lifecycle adapters.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution::lifecycle_adapter::{
    LifecycleAdapterInvocation, run_lifecycle_adapter,
};
use ryeos_external_execution_contract::runtime_snapshot::{
    MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES, RuntimeSnapshotAdapterRequest,
    RuntimeSnapshotAdapterResponse, RuntimeSnapshotReadinessObservation,
    RuntimeSnapshotReadinessRequest, RuntimeSnapshotQualificationAdapterRequest,
    RuntimeSnapshotQualificationAdapterResponse,
};
use ryeos_external_execution_contract::staging_package::GuestStagingExpected;
use ryeos_external_execution_contract::{
    AllocationReservation, BoundOccurrence, LIFECYCLE_ADAPTER_PROTOCOL, LIFECYCLE_BOOTSTRAP_FD_ENV,
    LIFECYCLE_CREDENTIAL_FD_ENV, LIFECYCLE_GUEST_PACKAGE_FD_ENV, LIFECYCLE_HOSTS_FD_ENV,
    LIFECYCLE_HOSTS_SHA256_ENV, LIFECYCLE_LAUNCHER_FD_ENV, LIFECYCLE_NETWORK_POLICY_SHA256_ENV,
    LIFECYCLE_PROVIDER_SPEC_FD_ENV, LIFECYCLE_PROVIDER_SPEC_SHA256_ENV,
    LIFECYCLE_REMAINING_TIMEOUT_MS_ENV, LIFECYCLE_RESOLVER_FD_ENV, LIFECYCLE_RESOLVER_SHA256_ENV,
    LIFECYCLE_SETTINGS_FD_ENV, LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV, LIFECYCLE_SIGNED_IMPORT_FD_ENV,
    LIFECYCLE_SUPERVISOR_FD_ENV, LifecycleAdapterInspectionRequest,
    LifecycleAdapterInspectionResponse, LifecycleAdapterRequest, LifecycleAdapterResponse,
    LifecycleArtifactInspection, LifecycleArtifactRole, LifecycleGuestPackageDelivery,
    LifecycleOperationCommon, LifecycleRuntimeProbeRequest, LifecycleRuntimeProbeResponse,
    LifecycleRuntimeProbeSource, MAX_LIFECYCLE_REQUEST_BYTES, MAX_LIFECYCLE_RESPONSE_BYTES,
    SupervisorActivationIntent, TerminationIntent, from_json_slice_strict,
};

use crate::external_artifacts::{
    CapturedLifecycleProviderSpec, ResolvedExternalLifecycleArtifacts,
};
use crate::external_placement::{
    ExternalAllocationResolution, ExternalLifecycleObservation, ExternalPlacementBackend,
    ExternalSupervisorActivation, ExternalSupervisorActivationResolution,
    ExternalTerminationResolution, SupervisorGuestPackage,
};
use crate::node_config::sections::external_execution::ExternalPlacementBackendContract;
use crate::runtime_db::external_execution::{
    ExternalAllocationOccurrence, ExternalAllocationReservation,
    ExternalGuestPackageDeliveryCommitment, ExternalSupervisorActivationIntent,
    ExternalTerminationIntent,
};
use crate::vault::placement::PlacementCredential;

// Inspection re-reads every declared executable through the transferred
// descriptor so the adapter independently proves the artifact set it will
// execute.  Debug and otherwise unstripped admitted artifacts can be large;
// the ordinary five-second command/control budget is therefore not a valid
// ceiling for this startup-only, non-provider-contact operation.  Keep it
// finite and aligned with the maximum admitted lifecycle contact window.
const LIFECYCLE_ADAPTER_INSPECTION_TIMEOUT_SECONDS: f64 = 60.0;

struct CapturedLifecycleNetworkInputs {
    resolver: lillux::secure_fs::CapturedRegularFile,
    hosts: lillux::secure_fs::CapturedRegularFile,
    resolver_sha256: String,
    hosts_sha256: String,
    policy_sha256: String,
}

fn capture_lifecycle_network_inputs(
    policy: &ryeos_state::external_execution::transport::ExternalNetworkInputPolicy,
) -> Result<CapturedLifecycleNetworkInputs> {
    use ryeos_state::external_execution::transport::{
        ExternalCapturedNetworkInputs, ExternalNetworkInputSelection,
    };

    policy.validate()?;
    let capture = |selection: &ExternalNetworkInputSelection| -> Result<
        lillux::secure_fs::CapturedRegularFile,
    > {
        let path = lillux::canonicalize_existing_path(std::path::Path::new(&selection.source))
            .with_context(|| "canonicalize signed lifecycle network input")?;
        let file = lillux::secure_fs::open_pinned_regular_file_no_follow(&path)
            .with_context(|| "open pinned lifecycle network input")?;
        let observation = file.observation()?;
        file.capture_sealed_bounded(&observation, selection.max_bytes)
            .with_context(|| "capture bounded lifecycle network input")
    };

    let resolver = capture(&policy.resolver)?;
    let hosts = capture(&policy.hosts)?;
    lillux::network::NetworkContext::from_config_bytes(resolver.bytes(), hosts.bytes())
        .context("validate captured lifecycle network inputs")?;
    let captured =
        ExternalCapturedNetworkInputs::from_bytes(policy, resolver.bytes(), hosts.bytes())?;
    captured.validate_for(policy)?;
    let policy_sha256 = policy.digest()?;

    Ok(CapturedLifecycleNetworkInputs {
        resolver,
        hosts,
        resolver_sha256: captured.resolver_digest,
        hosts_sha256: captured.hosts_digest,
        policy_sha256,
    })
}

#[derive(Debug)]
pub(crate) struct ExecutableExternalPlacementBackend {
    declaration: ryeos_external_execution_contract::ExternalLifecycleAdapterDeclaration,
    _bundle_manifest_digest: String,
    _signer_fingerprint: String,
    adapter_hash: String,
    adapter_bytes: u64,
    adapter: lillux::InheritedDescriptorAuthority,
    supervisor_hash: String,
    supervisor_bytes: u64,
    supervisor: lillux::InheritedDescriptorAuthority,
    launcher_hash: String,
    launcher_bytes: u64,
    launcher: lillux::InheritedDescriptorAuthority,
    restoration_verifier: Option<lillux::InheritedDescriptorAuthority>,
    restoration_verifier_hash: Option<String>,
    provider_spec: CapturedLifecycleProviderSpec,
    snapshot_production_spec: Option<CapturedLifecycleProviderSpec>,
    inspection: LifecycleAdapterInspectionResponse,
}

impl ExecutableExternalPlacementBackend {
    pub(crate) fn new(artifacts: ResolvedExternalLifecycleArtifacts) -> Result<Self> {
        let adapter_bytes = artifact_bytes(&artifacts.adapter)?;
        let supervisor_bytes = artifact_bytes(&artifacts.supervisor)?;
        let launcher_bytes = artifact_bytes(&artifacts.launcher)?;
        let restoration_verifier_bytes = artifacts.restoration_verifier.as_ref()
            .map(artifact_bytes).transpose()?;
        ensure!(
            artifacts.provider_spec.sha256 == artifacts.declaration.provider_spec.sha256,
            "captured lifecycle provider spec identity differs from its signed declaration"
        );
        let mut inspected_artifacts = BTreeMap::from([
            (LifecycleArtifactRole::Supervisor, LifecycleArtifactInspection {
                descriptor: artifacts.supervisor.handle.inherited_descriptor().map_err(anyhow::Error::msg)?,
                digest: artifacts.supervisor.identity.content_hash.clone(),
                bytes: supervisor_bytes,
            }),
            (LifecycleArtifactRole::Launcher, LifecycleArtifactInspection {
                descriptor: artifacts.launcher.handle.inherited_descriptor().map_err(anyhow::Error::msg)?,
                digest: artifacts.launcher.identity.content_hash.clone(),
                bytes: launcher_bytes,
            }),
        ]);
        if let Some(verifier) = &artifacts.restoration_verifier {
            inspected_artifacts.insert(
                LifecycleArtifactRole::RestorationVerifier,
                LifecycleArtifactInspection {
                    descriptor: verifier.handle.inherited_descriptor().map_err(anyhow::Error::msg)?,
                    digest: verifier.identity.content_hash.clone(),
                    bytes: restoration_verifier_bytes.context("restoration verifier has no size")?,
                },
            );
        }
        let request = LifecycleAdapterInspectionRequest {
            schema: 1,
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
            adapter_id: artifacts.declaration.id.clone(),
            adapter_artifact_hash: artifacts.adapter.identity.content_hash.clone(),
            settings_schema_digest: artifacts.declaration.settings_schema_digest.clone(),
            target: lillux::platform::current_binary_target()?.into(),
            declared_capabilities: artifacts.declaration.capabilities.clone(),
            provider_spec: LifecycleArtifactInspection {
                descriptor: artifacts
                    .provider_spec
                    .authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?,
                digest: artifacts.provider_spec.sha256.clone(),
                bytes: artifacts.provider_spec.bytes,
            },
            snapshot_production_spec: artifacts
                .snapshot_production_spec
                .as_ref()
                .map(|spec| -> Result<LifecycleArtifactInspection> {
                    Ok(LifecycleArtifactInspection {
                        descriptor: spec
                            .authority
                            .inherited_descriptor()
                            .map_err(anyhow::Error::msg)?,
                        digest: spec.sha256.clone(),
                        bytes: spec.bytes,
                    })
                })
                .transpose()?,
            artifacts: inspected_artifacts,
        };
        request.validate()?;
        let request_bytes = lifecycle_inspection_bytes(&request)?;
        let request_handle =
            lillux::sealed_memfd(c"ryeos-lifecycle-inspection-request", &request_bytes)
                .map_err(anyhow::Error::msg)?;
        let response = run_lifecycle_adapter(
            &artifacts.adapter.handle,
            LifecycleAdapterInvocation::Inspect,
            &request_handle,
            [
                Some(artifacts.supervisor.handle.clone()),
                Some(artifacts.launcher.handle.clone()),
                artifacts.restoration_verifier.as_ref().map(|item| item.handle.clone()),
                Some(artifacts.provider_spec.authority.clone()),
                artifacts
                    .snapshot_production_spec
                    .as_ref()
                    .map(|spec| spec.authority.clone()),
            ]
            .into_iter()
            .flatten()
            .collect(),
            Vec::new(),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs_f64(
                LIFECYCLE_ADAPTER_INSPECTION_TIMEOUT_SECONDS,
            )),
        )?;
        let inspection = decode_inspection_response(&response.bytes, &request)?;

        Ok(Self {
            declaration: artifacts.declaration,
            _bundle_manifest_digest: artifacts.bundle_manifest_digest,
            _signer_fingerprint: artifacts.signer_fingerprint,
            adapter_hash: artifacts.adapter.identity.content_hash,
            adapter_bytes,
            adapter: artifacts.adapter.handle,
            supervisor_hash: artifacts.supervisor.identity.content_hash,
            supervisor_bytes,
            supervisor: artifacts.supervisor.handle,
            launcher_hash: artifacts.launcher.identity.content_hash,
            launcher_bytes,
            launcher: artifacts.launcher.handle,
            restoration_verifier_hash: artifacts.restoration_verifier.as_ref()
                .map(|item| item.identity.content_hash.clone()),
            restoration_verifier: artifacts.restoration_verifier.map(|item| item.handle),
            provider_spec: artifacts.provider_spec,
            snapshot_production_spec: artifacts.snapshot_production_spec,
            inspection,
        })
    }

    fn invoke(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        request: &LifecycleAdapterRequest,
        bootstrap: Option<&ryeos_state::external_execution::transport::ExternalSupervisorBootstrap>,
        guest_inputs: Option<&ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority>,
        guest_package: Option<&lillux::InheritedDescriptorAuthority>,
        signed_import: Option<&ryeos_external_execution_contract::guest_import_authorization::SignedGuestImportAuthorization>,
        signed_assignment: Option<&ryeos_external_execution_contract::guest_import_authorization::SignedGuestOccurrenceAssignment>,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<LifecycleAdapterResponse>> {
        let deadline = deadline.min(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(u64::from(contract.contact_timeout_seconds)),
        ));
        ensure!(
            !deadline.has_elapsed(),
            "external lifecycle contact deadline expired"
        );
        // Sealed request/credential preparation also acquires descriptor
        // authority. Keep the bounded outer lease so nested preparation
        // cannot queue behind a new exclusive fork after budget admission.
        let descriptors = lillux::retain_fork_sensitive_descriptors_until(deadline)?;
        self.qualify_offline(contract, credential)?;
        let network_inputs =
            capture_lifecycle_network_inputs(&contract.controller_transport.network_inputs)?;
        let request_handle = lillux::sealed_memfd(
            c"ryeos-lifecycle-operation-request",
            &request.canonical_bytes()?,
        )
        .map_err(anyhow::Error::msg)?;
        let settings = lillux::canonical_json(&contract.settings)?;
        let settings_handle =
            lillux::sealed_memfd(c"ryeos-lifecycle-settings", settings.as_bytes())
                .map_err(anyhow::Error::msg)?;
        let credential_handle = lillux::sealed_memfd(
            c"ryeos-lifecycle-credential",
            credential.secret().as_bytes(),
        )
        .map_err(anyhow::Error::msg)?;
        let mut inherited = vec![
            self.supervisor.clone(),
            self.launcher.clone(),
            self.provider_spec.authority.clone(),
            settings_handle.clone(),
            credential_handle.clone(),
            network_inputs.resolver.authority().clone(),
            network_inputs.hosts.authority().clone(),
        ];
        let mut envs = vec![
            (
                LIFECYCLE_SUPERVISOR_FD_ENV.into(),
                self.supervisor
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_LAUNCHER_FD_ENV.into(),
                self.launcher
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_SETTINGS_FD_ENV.into(),
                settings_handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_CREDENTIAL_FD_ENV.into(),
                credential_handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_PROVIDER_SPEC_FD_ENV.into(),
                self.provider_spec
                    .authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_PROVIDER_SPEC_SHA256_ENV.into(),
                self.provider_spec.sha256.clone(),
            ),
            (
                LIFECYCLE_RESOLVER_FD_ENV.into(),
                network_inputs
                    .resolver
                    .authority()
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_HOSTS_FD_ENV.into(),
                network_inputs
                    .hosts
                    .authority()
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_RESOLVER_SHA256_ENV.into(),
                network_inputs.resolver_sha256.clone(),
            ),
            (
                LIFECYCLE_HOSTS_SHA256_ENV.into(),
                network_inputs.hosts_sha256.clone(),
            ),
            (
                LIFECYCLE_NETWORK_POLICY_SHA256_ENV.into(),
                network_inputs.policy_sha256.clone(),
            ),
        ];
        if let Some(bootstrap) = bootstrap {
            bootstrap.validate()?;
            let encoded = bootstrap.canonical_bytes()?;
            let handle = lillux::sealed_memfd(c"ryeos-lifecycle-supervisor-bootstrap", &encoded)
                .map_err(anyhow::Error::msg)?;
            envs.push((
                LIFECYCLE_BOOTSTRAP_FD_ENV.into(),
                handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ));
            inherited.push(handle);
        }
        if let Some(guest_inputs) = guest_inputs {
            ensure!(
                guest_inputs.projection().identity_digest()? == guest_inputs.identity_digest()?,
                "external guest input identity changed before adapter launch"
            );
            if let LifecycleAdapterRequest::ActivateSupervisor {
                guest_input_identity,
                guest_input_projection,
                ..
            } = request
            {
                ensure!(
                    guest_inputs.identity_digest()? == *guest_input_identity
                        && guest_inputs.projection() == guest_input_projection,
                    "external guest input identity changed at package handoff"
                );
            }
        }
        if let Some(package) = guest_package {
            envs.push((
                LIFECYCLE_GUEST_PACKAGE_FD_ENV.into(),
                package
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ));
            inherited.push(package.clone());
        }
        ensure!(
            signed_import.is_some() == guest_package.is_some()
                && signed_assignment.is_some() == signed_import.is_some()
                && signed_import.is_some()
                    == matches!(request, LifecycleAdapterRequest::ActivateSupervisor { .. }),
            "signed guest import and exact package must cross one activation contact together"
        );
        if let Some(signed) = signed_import {
            signed.validate_shape()?;
            let bytes = ryeos_external_execution_contract::canonical_json(signed)?;
            let handle = lillux::sealed_memfd(c"ryeos-signed-guest-import", &bytes)
                .map_err(anyhow::Error::msg)?;
            envs.push((
                LIFECYCLE_SIGNED_IMPORT_FD_ENV.into(),
                handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ));
            inherited.push(handle);
        }
        if let Some(signed) = signed_assignment {
            signed.validate_shape()?;
            let bytes = ryeos_external_execution_contract::canonical_json(signed)?;
            let handle = lillux::sealed_memfd(c"ryeos-signed-guest-assignment", &bytes)
                .map_err(anyhow::Error::msg)?;
            envs.push((
                LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV.into(),
                handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ));
            inherited.push(handle);
        }
        drop(descriptors);
        let remaining_timeout_ms = deadline.remaining().as_millis();
        ensure!(
            remaining_timeout_ms > 0,
            "external lifecycle contact deadline expired before spawn"
        );
        envs.push((
            LIFECYCLE_REMAINING_TIMEOUT_MS_ENV.into(),
            u64::try_from(remaining_timeout_ms)
                .context("external lifecycle remaining timeout exceeds its bound")?
                .to_string(),
        ));
        let response = run_lifecycle_adapter(
            &self.adapter,
            LifecycleAdapterInvocation::Operate,
            &request_handle,
            inherited,
            envs,
            deadline,
        )?;
        let deadline_exceeded = response.deadline_exceeded;
        let response = decode_operation_response(&response.bytes, request)?;
        Ok(ExternalLifecycleObservation {
            value: response,
            deadline_exceeded,
        })
    }
    fn invoke_snapshot_adapter(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request_bytes: &[u8],
        invocation: LifecycleAdapterInvocation,
        upload: Option<&lillux::InheritedDescriptorAuthority>,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ryeos_external_execution::lifecycle_adapter::LifecycleAdapterOutput> {
        let spec = self.snapshot_production_spec.as_ref()
            .context("installed lifecycle adapter has no signed snapshot production profile")?;
        self.invoke_sealed_snapshot_operation(
            binding.network_inputs(), spec, binding.settings(), binding.settings_digest(),
            credential, request_bytes, invocation, upload, deadline,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn invoke_sealed_snapshot_operation(
        &self,
        network_policy: &ryeos_state::external_execution::transport::ExternalNetworkInputPolicy,
        spec: &CapturedLifecycleProviderSpec,
        settings: &serde_json::Value,
        settings_digest: &str,
        credential: &PlacementCredential,
        request_bytes: &[u8],
        invocation: LifecycleAdapterInvocation,
        upload: Option<&lillux::InheritedDescriptorAuthority>,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ryeos_external_execution::lifecycle_adapter::LifecycleAdapterOutput> {
        ensure!(!deadline.has_elapsed(), "snapshot adapter deadline expired");
        ensure!(
            request_bytes.len() <= MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
            "snapshot adapter request exceeds its bound"
        );
        let descriptors = lillux::retain_fork_sensitive_descriptors_until(deadline)?;
        let network_inputs = capture_lifecycle_network_inputs(network_policy)?;
        let request_handle = lillux::sealed_memfd(c"ryeos-runtime-snapshot-request", request_bytes)
            .map_err(anyhow::Error::msg)?;
        let settings_bytes = lillux::canonical_json(settings)?;
        ensure!(
            lillux::sha256_hex(settings_bytes.as_bytes()) == settings_digest,
            "snapshot operation settings changed after node admission"
        );
        let settings_handle = lillux::sealed_memfd(
            c"ryeos-runtime-snapshot-settings",
            settings_bytes.as_bytes(),
        )
        .map_err(anyhow::Error::msg)?;
        let credential_handle = lillux::sealed_memfd(
            c"ryeos-runtime-snapshot-credential",
            credential.secret().as_bytes(),
        )
        .map_err(anyhow::Error::msg)?;
        let fd = |handle: &lillux::InheritedDescriptorAuthority| -> Result<String> {
            Ok(handle
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?
                .to_string())
        };
        let mut inherited = vec![
            spec.authority.clone(),
            settings_handle.clone(),
            credential_handle.clone(),
            network_inputs.resolver.authority().clone(),
            network_inputs.hosts.authority().clone(),
        ];
        if let Some(upload) = upload {
            inherited.push(upload.clone());
        }
        let environment = vec![
            (LIFECYCLE_PROVIDER_SPEC_FD_ENV.into(), fd(&spec.authority)?),
            (
                LIFECYCLE_PROVIDER_SPEC_SHA256_ENV.into(),
                spec.sha256.clone(),
            ),
            (LIFECYCLE_SETTINGS_FD_ENV.into(), fd(&settings_handle)?),
            (LIFECYCLE_CREDENTIAL_FD_ENV.into(), fd(&credential_handle)?),
            (
                LIFECYCLE_RESOLVER_FD_ENV.into(),
                fd(network_inputs.resolver.authority())?,
            ),
            (
                LIFECYCLE_HOSTS_FD_ENV.into(),
                fd(network_inputs.hosts.authority())?,
            ),
            (
                LIFECYCLE_RESOLVER_SHA256_ENV.into(),
                network_inputs.resolver_sha256,
            ),
            (
                LIFECYCLE_HOSTS_SHA256_ENV.into(),
                network_inputs.hosts_sha256,
            ),
            (
                LIFECYCLE_NETWORK_POLICY_SHA256_ENV.into(),
                network_inputs.policy_sha256,
            ),
            (
                LIFECYCLE_REMAINING_TIMEOUT_MS_ENV.into(),
                u64::try_from(deadline.remaining().as_millis())?.to_string(),
            ),
        ];
        drop(descriptors);
        run_lifecycle_adapter(
            &self.adapter,
            invocation,
            &request_handle,
            inherited,
            environment,
            deadline,
        )
    }
}

impl ExternalPlacementBackend for ExecutableExternalPlacementBackend {
    fn backend_id(&self) -> &str {
        &self.declaration.id
    }

    fn artifact_hash(&self) -> &str {
        &self.adapter_hash
    }

    fn artifact_bytes(&self) -> u64 {
        self.adapter_bytes
    }

    fn supervisor_artifact(&self) -> (&str, u64) {
        (&self.supervisor_hash, self.supervisor_bytes)
    }

    fn launcher_artifact(&self) -> (&str, u64) {
        (&self.launcher_hash, self.launcher_bytes)
    }

    fn settings_schema_digest(&self) -> &str {
        &self.declaration.settings_schema_digest
    }

    fn preflight_runtime_snapshot(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
    ) -> Result<()> {
        let spec = self
            .snapshot_production_spec
            .as_ref()
            .context("installed lifecycle adapter has no signed snapshot production profile")?;
        ensure!(
            binding.backend() == self.declaration.id
                && binding.adapter_artifact_hash() == self.adapter_hash
                && binding.snapshot_spec_sha256() == spec.sha256
                && self
                    .inspection
                    .observed_snapshot_production_spec_sha256
                    .as_deref()
                    == Some(spec.sha256.as_str())
                && credential.backend() == binding.backend()
                && credential.account() == binding.account()
                && lillux::sha256_hex(lillux::canonical_json(binding.settings())?.as_bytes())
                    == binding.settings_digest(),
            "snapshot producer differs from its inspected signed authority"
        );
        Ok(())
    }

    fn preflight_snapshot_qualification_create(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        credential: &PlacementCredential,
    ) -> Result<()> {
        ensure!(
            producer.backend() == self.declaration.id
                && producer.adapter_artifact_hash() == self.adapter_hash
                && qualification.production_binding_id() == producer.id()
                && qualification.production_binding_digest() == producer.digest()
                && qualification.provider_spec_digest() == self.provider_spec.sha256
                && self.restoration_verifier_hash.as_deref()
                    == Some(qualification.verifier_artifact_hash())
                && self.restoration_verifier.is_some()
                && self.inspection.observed_provider_spec_sha256 == self.provider_spec.sha256
                && credential.backend() == producer.backend()
                && credential.account() == producer.account()
                && lillux::sha256_hex(lillux::canonical_json(qualification.settings())?.as_bytes())
                    == qualification.settings_digest(),
            "snapshot qualification differs from its exact signed producer or provider profile"
        );
        Ok(())
    }

    fn create_snapshot_qualification_occurrence(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotQualificationAdapterRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotQualificationAdapterResponse>> {
        self.preflight_snapshot_qualification_create(producer, qualification, credential)?;
        request.validate()?;
        ensure!(
            request.intent.qualification_profile_digest == qualification.digest()
                && request.intent.snapshot_operation_id == request.source_intent.operation_id
                && request.intent.provider_id == producer.backend()
                && request.intent.adapter_artifact_hash == self.adapter_hash
                && request.intent.provider_spec_digest == self.provider_spec.sha256
                && request.intent.settings_digest == qualification.settings_digest()
                && request.intent.verifier_artifact_hash == qualification.verifier_artifact_hash()
                && request.intent.maximum_lifetime_seconds == qualification.maximum_lifetime_seconds()
                && request.source_intent.production_profile_digest == producer.digest(),
            "qualification create differs from inspected signed authority"
        );
        let remaining_ms = request.intent.attempt_deadline_ms
            .saturating_sub(lillux::time::timestamp_millis());
        ensure!(remaining_ms > 0, "qualification create expired before adapter contact");
        let deadline = deadline
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(u64::from(qualification.contact_timeout_seconds())),
            ))
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
            ));
        let bytes = ryeos_external_execution_contract::canonical_json(request)?;
        let output = self.invoke_sealed_snapshot_operation(
            producer.network_inputs(), &self.provider_spec, qualification.settings(),
            qualification.settings_digest(), credential, &bytes,
            LifecycleAdapterInvocation::QualifySnapshotCreate, None, deadline,
        )?;
        let value: RuntimeSnapshotQualificationAdapterResponse =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid qualification create adapter response"))?;
        value.validate_for(request)
            .map_err(|_| anyhow::anyhow!("invalid qualification create adapter response"))?;
        if let RuntimeSnapshotQualificationAdapterResponse::OccurrenceBound { occurrence } = &value {
            ensure!(
                !occurrence.contact_deadline_exceeded,
                "qualification adapter claimed daemon-only deadline evidence"
            );
        }
        Ok(ExternalLifecycleObservation { value, deadline_exceeded: output.deadline_exceeded })
    }

    fn produce_runtime_snapshot(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotAdapterRequest,
        upload: &lillux::InheritedDescriptorAuthority,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotAdapterResponse>> {
        self.preflight_runtime_snapshot(binding, credential)?;
        request.validate()?;
        let spec = self
            .snapshot_production_spec
            .as_ref()
            .context("installed lifecycle adapter has no signed snapshot production profile")?;
        ensure!(
            binding.backend() == self.declaration.id
                && binding.adapter_artifact_hash() == self.adapter_hash
                && binding.snapshot_spec_sha256() == spec.sha256
                && self
                    .inspection
                    .observed_snapshot_production_spec_sha256
                    .as_deref()
                    == Some(spec.sha256.as_str())
                && request.intent.provider_id == self.declaration.id
                && request.intent.adapter_artifact_hash == self.adapter_hash
                && request.provider_spec_digest == spec.sha256
                && request.intent.production_profile_digest == binding.digest()
                && request.intent.settings_digest == binding.settings_digest()
                && credential.backend() == binding.backend()
                && credential.account() == binding.account()
                && request.upload_bytes <= binding.maximum_upload_bytes()
                && request.upload_descriptor
                    == upload.inherited_descriptor().map_err(anyhow::Error::msg)?,
            "snapshot attempt differs from its inspected signed producer authority"
        );
        let now = lillux::time::timestamp_millis();
        let remaining_ms = request.intent.attempt_deadline_ms.saturating_sub(now);
        ensure!(
            remaining_ms > 0,
            "snapshot attempt expired before adapter contact"
        );
        let deadline = deadline
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(u64::from(binding.contact_timeout_seconds())),
            ))
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
            ));
        let request_bytes = ryeos_external_execution_contract::canonical_json(request)?;
        let output = self.invoke_snapshot_adapter(
            binding,
            credential,
            &request_bytes,
            LifecycleAdapterInvocation::ProduceSnapshot,
            Some(upload),
            deadline,
        )?;
        let value: RuntimeSnapshotAdapterResponse =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid runtime snapshot adapter response"))?;
        value
            .validate_for(request)
            .map_err(|_| anyhow::anyhow!("invalid runtime snapshot adapter response"))?;
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn observe_runtime_snapshot_readiness(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotReadinessRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotReadinessObservation>> {
        self.preflight_runtime_snapshot(binding, credential)?;
        request.validate()?;
        ensure!(
            request.intent.provider_id == self.declaration.id
                && request.intent.adapter_artifact_hash == self.adapter_hash
                && request.intent.production_profile_digest == binding.digest()
                && request.intent.settings_digest == binding.settings_digest()
                && request.provider_spec_digest == binding.snapshot_spec_sha256(),
            "snapshot readiness differs from its inspected signed producer authority"
        );
        let deadline = deadline.min(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(u64::from(binding.contact_timeout_seconds())),
        ));
        let request_bytes = ryeos_external_execution_contract::canonical_json(request)?;
        let output = self.invoke_snapshot_adapter(
            binding,
            credential,
            &request_bytes,
            LifecycleAdapterInvocation::ObserveSnapshotReadiness,
            None,
            deadline,
        )?;
        let value: RuntimeSnapshotReadinessObservation =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid runtime snapshot readiness response"))?;
        value
            .validate_for(request)
            .map_err(|_| anyhow::anyhow!("invalid runtime snapshot readiness response"))?;
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn lifecycle_capabilities(
        &self,
    ) -> std::collections::BTreeSet<ryeos_external_execution_contract::LifecycleCapability> {
        self.inspection.effective_capabilities.clone()
    }

    fn qualify_offline(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
    ) -> Result<()> {
        ensure!(
            contract.backend == self.declaration.id
                && contract.backend_artifact_hash == self.adapter_hash
                && contract.backend_artifact_bytes == self.adapter_bytes
                && contract.supervisor_artifact_hash == self.supervisor_hash
                && contract.supervisor_artifact_bytes == self.supervisor_bytes
                && contract.launcher_artifact_hash == self.launcher_hash
                && contract.launcher_artifact_bytes == self.launcher_bytes
                && contract.settings_schema_digest == self.declaration.settings_schema_digest
                && self.inspection.effective_capabilities == self.declaration.capabilities
                && credential.backend() == contract.backend
                && credential.account() == contract.account,
            "signed external lifecycle generation contradicts the protected placement binding"
        );
        Ok(())
    }

    fn verify_runtime_probe(
        &self,
        contract: &ExternalPlacementBackendContract,
        proof: &ryeos_state::external_content::products::composition::AdmittedProductQualification,
        source: &ryeos_external_execution::guest_runtime_product::GuestOwnerRuntimeManifestIdentity,
        binding_hash: &str,
    ) -> Result<()> {
        ensure!(
            contract.backend == self.declaration.id
                && contract.backend_artifact_hash == self.adapter_hash
                && contract.backend_artifact_bytes == self.adapter_bytes
                && contract.settings_schema_digest == self.declaration.settings_schema_digest
                && source.manifest_hash == contract.guest_runtime_manifest_hash
                && source.manifest_hash == proof.evidence.result.subject_manifest_hash,
            "runtime probe changed its signed adapter or authenticated product"
        );
        let settings_bytes = ryeos_external_execution_contract::canonical_json(&contract.settings)?;
        ensure!(
            lillux::sha256_hex(&settings_bytes) == contract.settings_digest,
            "runtime probe settings changed their signed digest"
        );
        let request = LifecycleRuntimeProbeRequest {
            schema: 1,
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
            adapter_id: self.declaration.id.clone(),
            adapter_artifact_hash: self.adapter_hash.clone(),
            settings_digest: contract.settings_digest.clone(),
            binding_hash: binding_hash.to_owned(),
            qualification_attestation_hash: proof.attestation_hash.clone(),
            product_witness_hash: proof.evidence.product_witness_hash.clone(),
            account: contract.account.clone(),
            source: LifecycleRuntimeProbeSource {
                manifest_hash: source.manifest_hash.clone(),
                owner_executable_sha256: source.owner_executable_sha256.clone(),
                controller_root_blob_sha256: source.controller_root_blob_sha256.clone(),
                controller_public_root: source.controller_public_root.clone(),
            },
            probe_evidence: proof.evidence.result.probe_evidence.clone(),
        };
        let request_handle = lillux::sealed_memfd(
            c"ryeos-lifecycle-runtime-probe-request",
            &request.canonical_bytes()?,
        )
        .map_err(anyhow::Error::msg)?;
        let settings_handle =
            lillux::sealed_memfd(c"ryeos-lifecycle-runtime-probe-settings", &settings_bytes)
                .map_err(anyhow::Error::msg)?;
        let deadline = lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs_f64(LIFECYCLE_ADAPTER_INSPECTION_TIMEOUT_SECONDS),
        );
        let response = run_lifecycle_adapter(
            &self.adapter,
            LifecycleAdapterInvocation::VerifyRuntimeProbe,
            &request_handle,
            vec![settings_handle.clone()],
            vec![(
                LIFECYCLE_SETTINGS_FD_ENV.into(),
                settings_handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            )],
            deadline,
        )?;
        let decoded: LifecycleRuntimeProbeResponse =
            from_json_slice_strict(&response.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid lifecycle runtime probe response"))?;
        decoded
            .validate_for(&request)
            .map_err(|_| anyhow::anyhow!("invalid lifecycle runtime probe response"))
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_guest_package(
        &self,
        contract: &ExternalPlacementBackendContract,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        guest_inputs: &ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
        activation_request_digest: &str,
        parent: &lillux::PinnedDirectory,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<SupervisorGuestPackage> {
        let bootstrap_bytes = bootstrap.canonical_bytes()?;
        let guest_input_identity = guest_inputs.identity_digest()?;
        let bootstrap_sha256 = lillux::sha256_hex(&bootstrap_bytes);
        let bootstrap_file =
            lillux::sealed_memfd(c"ryeos-external-guest-bootstrap", &bootstrap_bytes)
                .map_err(anyhow::Error::msg)?;
        let expected = GuestStagingExpected {
            inputs: guest_inputs.projection(),
            activation_request_digest,
            bootstrap_sha256: &bootstrap_sha256,
            supervisor_sha256: &self.supervisor_hash,
            launcher_sha256: &self.launcher_hash,
            maximum_regular_bytes: contract.max_guest_package_regular_bytes,
            maximum_framed_bytes: contract.max_guest_package_framed_bytes,
        };
        let package =
            ryeos_external_execution::guest_package_producer::prepare_private_guest_package(
                parent,
                guest_inputs,
                &bootstrap_file,
                &self.supervisor,
                &self.launcher,
                &expected,
                deadline,
            )?;
        let commitment = ExternalGuestPackageDeliveryCommitment {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            activation_request_digest: activation_request_digest.to_owned(),
            guest_input_identity,
            manifest_sha256: package.manifest_sha256().to_owned(),
            payload_sha256: package.sha256().to_owned(),
            regular_bytes: package.manifest().total_regular_bytes,
            framed_bytes: package.bytes(),
        };
        Ok(SupervisorGuestPackage::Prepared {
            package,
            commitment,
        })
    }

    fn allocate(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalAllocationResolution>> {
        self.allocation_operation(contract, credential, reservation, false, deadline)
    }

    fn reconcile_allocation(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalAllocationResolution>> {
        self.allocation_operation(contract, credential, reservation, true, deadline)
    }

    fn activate_supervisor(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalSupervisorActivationIntent,
        activation: &ExternalSupervisorActivation,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalSupervisorActivationResolution>> {
        self.activation_operation(
            contract,
            credential,
            reservation,
            occurrence,
            intent,
            Some(activation),
            false,
            deadline,
        )
    }

    fn reconcile_supervisor_activation(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalSupervisorActivationIntent,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalSupervisorActivationResolution>> {
        self.activation_operation(
            contract,
            credential,
            reservation,
            occurrence,
            intent,
            None,
            true,
            deadline,
        )
    }

    fn terminate(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalTerminationIntent,
    ) -> Result<ExternalTerminationResolution> {
        self.termination_operation(contract, credential, reservation, occurrence, intent, false)
    }

    fn reconcile_termination(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalTerminationIntent,
    ) -> Result<ExternalTerminationResolution> {
        self.termination_operation(contract, credential, reservation, occurrence, intent, true)
    }
}

impl ExecutableExternalPlacementBackend {
    fn common(
        contract: &ExternalPlacementBackendContract,
        binding_hash: &str,
        operation_id: &str,
    ) -> LifecycleOperationCommon {
        LifecycleOperationCommon {
            schema: 1,
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
            operation_id: operation_id.into(),
            binding_hash: binding_hash.into(),
            settings_digest: contract.settings_digest.clone(),
        }
    }

    fn allocation_operation(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        reconcile: bool,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalAllocationResolution>> {
        let reservation_wire = AllocationReservation {
            placement_thread_id: reservation.placement_thread_id.clone(),
            admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
            base_snapshot_hash: reservation.base_snapshot_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            maximum_lifetime_seconds: reservation.timeout_seconds,
            contact_deadline_ms: reservation.contact_deadline_ms,
        };
        let common = Self::common(
            contract,
            &reservation.binding_hash,
            &reservation.request_digest,
        );
        let request = if reconcile {
            LifecycleAdapterRequest::ReconcileAllocation {
                common,
                reservation: reservation_wire,
            }
        } else {
            LifecycleAdapterRequest::Allocate {
                common,
                reservation: reservation_wire,
            }
        };
        let response = self.invoke(
            contract, credential, &request, None, None, None, None, None, deadline,
        )?;
        let value = match response.value {
            LifecycleAdapterResponse::AllocationBound {
                occurrence_id,
                provider_observation_digest,
                ..
            } => ExternalAllocationResolution::Bound {
                occurrence_id,
                provider_observation_digest,
            },
            LifecycleAdapterResponse::AllocationNoOccurrence {
                provider_observation_digest,
                ..
            } => ExternalAllocationResolution::NoOccurrence {
                provider_observation_digest,
            },
            LifecycleAdapterResponse::AllocationPending { .. } => {
                ExternalAllocationResolution::Pending
            }
            _ => anyhow::bail!("lifecycle adapter returned a non-allocation response"),
        };
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: response.deadline_exceeded,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn activation_operation(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalSupervisorActivationIntent,
        activation_authority: Option<&ExternalSupervisorActivation>,
        reconcile: bool,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalSupervisorActivationResolution>> {
        let occurrence_wire = BoundOccurrence {
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
        };
        let activation = SupervisorActivationIntent {
            activation_request_digest: intent.activation_request_digest.clone(),
            supervisor_runtime_hash: intent.supervisor_runtime_hash.clone(),
            launcher_artifact_hash: contract.launcher_artifact_hash.clone(),
            attachment_deadline_ms: intent.attachment_deadline_ms,
            execution_timeout_seconds: intent.execution_timeout_seconds,
            post_execution_timeout_seconds: intent.post_execution_timeout_seconds,
            channel_max_bytes: intent.channel_max_bytes,
        };
        let common = Self::common(
            contract,
            &reservation.binding_hash,
            &intent.activation_request_digest,
        );
        let (package_authority, import_ticket) = if reconcile {
            (None, None)
        } else {
            let activation_authority = activation_authority
                .context("first lifecycle activation has no guest package authority")?;
            let package = activation_authority.prepared_guest_package()?;
            ensure!(
                package.sha256() == intent.delivery.payload_sha256
                    && package.manifest_sha256() == intent.delivery.manifest_sha256
                    && package.bytes() == intent.delivery.framed_bytes
                    && package.manifest().total_regular_bytes == intent.delivery.regular_bytes,
                "prepared guest package changed after durable activation claim"
            );
            let signed = activation_authority.signed_import_authorization()?;
            let authorization = &signed.authorization;
            ensure!(
                authorization.placement_thread_id == reservation.placement_thread_id
                    && authorization.admitted_capsule_hash == reservation.admitted_capsule_hash
                    && authorization.base_snapshot_hash == reservation.base_snapshot_hash
                    && authorization.execution_binding_hash == reservation.binding_hash
                    && authorization.activation_request_digest == intent.activation_request_digest
                    && authorization.supervisor_runtime_hash == intent.supervisor_runtime_hash
                    && authorization.guest_runtime_manifest_hash
                        == contract.guest_runtime_manifest_hash
                    && authorization.guest_inputs
                        == *activation_authority.guest_inputs().projection(),
                "signed guest import changed the admitted activation"
            );
            let ticket = authorization.ticket.clone();
            ensure!(
                ticket.guest_input_identity == intent.guest_input_identity
                    && ticket.payload_sha256 == intent.delivery.payload_sha256
                    && ticket.manifest_sha256 == intent.delivery.manifest_sha256
                    && ticket.regular_bytes == intent.delivery.regular_bytes
                    && ticket.framed_bytes == intent.delivery.framed_bytes
                    && ticket.bootstrap_sha256
                        == lillux::sha256_hex(&activation_authority.bootstrap().canonical_bytes()?)
                    && ticket.supervisor_sha256 == contract.supervisor_artifact_hash
                    && ticket.launcher_sha256 == contract.launcher_artifact_hash,
                "signed guest import ticket changed durable delivery or executable roots"
            );
            ticket.validate_verified_manifest(
                &ryeos_external_execution_contract::staging_package::GuestImportContext {
                    binding_hash: &reservation.binding_hash,
                    allocation_request_digest: &reservation.request_digest,
                    occurrence_id: &occurrence.occurrence_id,
                    activation_request_digest: &intent.activation_request_digest,
                },
                package.manifest(),
                package.sha256(),
                package.bytes(),
            )?;
            (Some(package.delivery_descriptor()?), Some(ticket))
        };
        let request = if reconcile {
            LifecycleAdapterRequest::ReconcileSupervisorActivation {
                common,
                occurrence: occurrence_wire,
                activation,
            }
        } else {
            LifecycleAdapterRequest::ActivateSupervisor {
                common,
                occurrence: occurrence_wire,
                activation,
                guest_input_identity: intent.guest_input_identity.clone(),
                guest_input_projection: activation_authority
                    .context("first activation lost its guest input authority")?
                    .guest_inputs()
                    .projection()
                    .clone(),
                import_ticket: import_ticket.context("first activation lost its import ticket")?,
                guest_package: LifecycleGuestPackageDelivery {
                    descriptor: package_authority
                        .as_ref()
                        .expect("first activation owns a prepared package")
                        .inherited_descriptor()
                        .map_err(anyhow::Error::msg)?,
                    payload_sha256: intent.delivery.payload_sha256.clone(),
                    manifest_sha256: intent.delivery.manifest_sha256.clone(),
                    regular_bytes: intent.delivery.regular_bytes,
                    framed_bytes: intent.delivery.framed_bytes,
                },
            }
        };
        let bootstrap = activation_authority.map(ExternalSupervisorActivation::bootstrap);
        let guest_inputs = activation_authority.map(ExternalSupervisorActivation::guest_inputs);
        let response = self.invoke(
            contract,
            credential,
            &request,
            bootstrap,
            guest_inputs,
            package_authority.as_ref(),
            activation_authority
                .map(ExternalSupervisorActivation::signed_import_authorization)
                .transpose()?,
            activation_authority
                .map(ExternalSupervisorActivation::signed_assignment)
                .transpose()?,
            deadline,
        )?;
        let value = match response.value {
            LifecycleAdapterResponse::SupervisorStarted {
                provider_observation_digest,
                ..
            } => ExternalSupervisorActivationResolution::Started {
                provider_observation_digest,
            },
            LifecycleAdapterResponse::SupervisorNotStarted {
                provider_observation_digest,
                ..
            } => ExternalSupervisorActivationResolution::NotStarted {
                provider_observation_digest,
            },
            LifecycleAdapterResponse::SupervisorPending { .. } => {
                ExternalSupervisorActivationResolution::Pending
            }
            _ => anyhow::bail!("lifecycle adapter returned a non-activation response"),
        };
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: response.deadline_exceeded,
        })
    }

    fn termination_operation(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalTerminationIntent,
        reconcile: bool,
    ) -> Result<ExternalTerminationResolution> {
        let common = Self::common(
            contract,
            &reservation.binding_hash,
            &intent.termination_request_digest,
        );
        let occurrence_wire = BoundOccurrence {
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
        };
        let termination = TerminationIntent {
            termination_request_digest: intent.termination_request_digest.clone(),
        };
        let request = if reconcile {
            LifecycleAdapterRequest::ReconcileTermination {
                common,
                occurrence: occurrence_wire,
                termination,
            }
        } else {
            LifecycleAdapterRequest::Terminate {
                common,
                occurrence: occurrence_wire,
                termination,
            }
        };
        let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
            u64::from(contract.cleanup_timeout_seconds),
        ));
        // Late terminal evidence may settle cleanup, never start execution.
        match self
            .invoke(
                contract, credential, &request, None, None, None, None, None, deadline,
            )?
            .value
        {
            LifecycleAdapterResponse::OccurrenceTerminal {
                provider_observation_digest,
                ..
            } => Ok(ExternalTerminationResolution::Terminal {
                provider_observation_digest,
            }),
            LifecycleAdapterResponse::TerminationPending { .. } => {
                Ok(ExternalTerminationResolution::Pending)
            }
            _ => anyhow::bail!("lifecycle adapter returned a non-termination response"),
        }
    }
}

fn decode_inspection_response(
    bytes: &[u8],
    request: &LifecycleAdapterInspectionRequest,
) -> Result<LifecycleAdapterInspectionResponse> {
    let response: LifecycleAdapterInspectionResponse =
        from_json_slice_strict(bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
            .map_err(|_| anyhow::anyhow!("invalid lifecycle adapter inspection response"))?;
    response
        .validate_for(request)
        .map_err(|_| anyhow::anyhow!("invalid lifecycle adapter inspection response"))?;
    Ok(response)
}

fn decode_operation_response(
    bytes: &[u8],
    request: &LifecycleAdapterRequest,
) -> Result<LifecycleAdapterResponse> {
    let response: LifecycleAdapterResponse =
        from_json_slice_strict(bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
            .map_err(|_| anyhow::anyhow!("invalid lifecycle adapter operation response"))?;
    response
        .validate_for(request)
        .map_err(|_| anyhow::anyhow!("invalid lifecycle adapter operation response"))?;
    Ok(response)
}

fn lifecycle_inspection_bytes(request: &LifecycleAdapterInspectionRequest) -> Result<Vec<u8>> {
    request.validate()?;
    let bytes = ryeos_external_execution_contract::canonical_json(request)?;
    ensure!(
        bytes.len() <= MAX_LIFECYCLE_REQUEST_BYTES,
        "lifecycle adapter inspection request exceeds its byte bound"
    );
    Ok(bytes)
}

fn artifact_bytes(artifact: &ryeos_engine::binary_resolver::CapturedExecutable) -> Result<u64> {
    let observation = artifact.handle.regular_file_observation()?;
    let bytes = observation.size();
    ensure!(
        (1..=1024 * 1024 * 1024).contains(&bytes),
        "external lifecycle artifact exceeds its byte bound"
    );
    ensure!(
        artifact
            .handle
            .digest_regular_file_stable_exact(&observation)?
            == artifact.identity.content_hash,
        "external lifecycle artifact changed after signed resolution"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn require_private_decode_error(error: anyhow::Error) {
        let error = error.context("lifecycle dispatch failed");
        for rendered in [
            format!("{error}"),
            format!("{error:#}"),
            format!("{error:?}"),
        ] {
            assert!(!rendered.contains("ADAPTER_SECRET_SENTINEL"), "{rendered}");
        }
        assert_eq!(
            error.chain().count(),
            2,
            "decoder source must not escape closed boundary"
        );
    }

    #[test]
    fn operation_decode_and_validation_never_expose_adapter_values() {
        let request = LifecycleAdapterRequest::Allocate {
            common: LifecycleOperationCommon {
                schema: 1,
                protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
                operation_id: "a".repeat(64),
                binding_hash: "b".repeat(64),
                settings_digest: "c".repeat(64),
            },
            reservation: AllocationReservation {
                placement_thread_id: "T-fixture".into(),
                admitted_capsule_hash: "d".repeat(64),
                base_snapshot_hash: "e".repeat(64),
                request_digest: "a".repeat(64),
                maximum_lifetime_seconds: 60,
                contact_deadline_ms: 100_000,
            },
        };
        let valid = serde_json::json!({"outcome":"allocation_pending", "operation_id":"a".repeat(64), "request_digest":"a".repeat(64)});
        decode_operation_response(&serde_json::to_vec(&valid).unwrap(), &request).unwrap();
        for fault in ["variant", "field", "identity"] {
            let mut value = valid.clone();
            match fault {
                "variant" => value["outcome"] = "ADAPTER_SECRET_SENTINEL".into(),
                "field" => value["ADAPTER_SECRET_SENTINEL"] = true.into(),
                "identity" => value["operation_id"] = "ADAPTER_SECRET_SENTINEL".into(),
                _ => unreachable!(),
            }
            require_private_decode_error(
                decode_operation_response(&serde_json::to_vec(&value).unwrap(), &request)
                    .unwrap_err(),
            );
        }
    }

    #[test]
    fn inspection_decode_and_validation_never_expose_adapter_values() {
        let request = LifecycleAdapterInspectionRequest {
            schema: 1, protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(), adapter_id: "fixture".into(),
            adapter_artifact_hash: "a".repeat(64), settings_schema_digest: "b".repeat(64),
            target: "x86_64-unknown-linux-gnu".into(),
            declared_capabilities: std::collections::BTreeSet::from([ryeos_external_execution_contract::LifecycleCapability::ExactAllocationReconciliation]),
            provider_spec: LifecycleArtifactInspection {
                descriptor: 22,
                digest: "e".repeat(64),
                bytes: 128,
            },
            snapshot_production_spec: Some(LifecycleArtifactInspection {
                descriptor: 23,
                digest: "f".repeat(64),
                bytes: 256,
            }),
            artifacts: BTreeMap::from([
                (LifecycleArtifactRole::Supervisor, LifecycleArtifactInspection { descriptor: 20, digest: "c".repeat(64), bytes: 4096 }),
                (LifecycleArtifactRole::Launcher, LifecycleArtifactInspection { descriptor: 21, digest: "d".repeat(64), bytes: 4096 }),
            ]),
        };
        let response = LifecycleAdapterInspectionResponse {
            schema: 1,
            protocol: request.protocol.clone(),
            adapter_id: request.adapter_id.clone(),
            adapter_build: "fixture-build".into(),
            observed_adapter_artifact_hash: request.adapter_artifact_hash.clone(),
            observed_settings_schema_digest: request.settings_schema_digest.clone(),
            target: request.target.clone(),
            effective_capabilities: request.declared_capabilities.clone(),
            observed_provider_spec_sha256: request.provider_spec.digest.clone(),
            observed_snapshot_production_spec_sha256: request
                .snapshot_production_spec
                .as_ref()
                .map(|spec| spec.digest.clone()),
            artifacts: request.artifacts.clone(),
        };
        let valid = serde_json::to_value(response).unwrap();
        decode_inspection_response(&serde_json::to_vec(&valid).unwrap(), &request).unwrap();
        let mut wrong_snapshot = valid.clone();
        wrong_snapshot["observed_snapshot_production_spec_sha256"] =
            serde_json::json!("0".repeat(64));
        require_private_decode_error(
            decode_inspection_response(&serde_json::to_vec(&wrong_snapshot).unwrap(), &request)
                .unwrap_err(),
        );
        for fault in ["variant", "field", "identity"] {
            let mut value = valid.clone();
            match fault {
                "variant" => {
                    value["effective_capabilities"] = serde_json::json!(["ADAPTER_SECRET_SENTINEL"])
                }
                "field" => value["ADAPTER_SECRET_SENTINEL"] = true.into(),
                "identity" => value["adapter_id"] = "ADAPTER_SECRET_SENTINEL".into(),
                _ => unreachable!(),
            }
            require_private_decode_error(
                decode_inspection_response(&serde_json::to_vec(&value).unwrap(), &request)
                    .unwrap_err(),
            );
        }
    }
}
