//! Closed executable runner for signed external occurrence lifecycle adapters.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution::lifecycle_adapter::{
    LifecycleAdapterInvocation, run_lifecycle_adapter,
};
use ryeos_external_execution_contract::{
    AllocationReservation, BoundOccurrence, LIFECYCLE_ADAPTER_PROTOCOL, LIFECYCLE_BOOTSTRAP_FD_ENV,
    LIFECYCLE_CREDENTIAL_FD_ENV, LIFECYCLE_LAUNCHER_FD_ENV, LIFECYCLE_SETTINGS_FD_ENV,
    LIFECYCLE_SUPERVISOR_FD_ENV, LifecycleAdapterInspectionRequest,
    LifecycleAdapterInspectionResponse, LifecycleAdapterRequest, LifecycleAdapterResponse,
    LifecycleArtifactInspection, LifecycleArtifactRole, LifecycleOperationCommon,
    MAX_LIFECYCLE_REQUEST_BYTES, MAX_LIFECYCLE_RESPONSE_BYTES, SupervisorActivationIntent,
    TerminationIntent, from_json_slice_strict,
};

use crate::external_artifacts::ResolvedExternalLifecycleArtifacts;
use crate::external_placement::{
    ExternalAllocationResolution, ExternalLifecycleObservation, ExternalPlacementBackend,
    ExternalSupervisorActivation, ExternalSupervisorActivationResolution,
    ExternalTerminationResolution,
};
use crate::node_config::sections::external_execution::ExternalPlacementBackendContract;
use crate::runtime_db::external_execution::{
    ExternalAllocationOccurrence, ExternalAllocationReservation,
    ExternalSupervisorActivationIntent, ExternalTerminationIntent,
};
use crate::vault::placement::PlacementCredential;

// Inspection re-reads every declared executable through the transferred
// descriptor so the adapter independently proves the artifact set it will
// execute.  Debug and otherwise unstripped admitted artifacts can be large;
// the ordinary five-second command/control budget is therefore not a valid
// ceiling for this startup-only, non-provider-contact operation.  Keep it
// finite and aligned with the maximum admitted lifecycle contact window.
const LIFECYCLE_ADAPTER_INSPECTION_TIMEOUT_SECONDS: f64 = 60.0;

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
    inspection: LifecycleAdapterInspectionResponse,
}

impl ExecutableExternalPlacementBackend {
    pub(crate) fn new(artifacts: ResolvedExternalLifecycleArtifacts) -> Result<Self> {
        let adapter_bytes = artifact_bytes(&artifacts.adapter)?;
        let supervisor_bytes = artifact_bytes(&artifacts.supervisor)?;
        let launcher_bytes = artifact_bytes(&artifacts.launcher)?;
        let request = LifecycleAdapterInspectionRequest {
            schema: 1,
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
            adapter_id: artifacts.declaration.id.clone(),
            adapter_artifact_hash: artifacts.adapter.identity.content_hash.clone(),
            settings_schema_digest: artifacts.declaration.settings_schema_digest.clone(),
            target: lillux::platform::current_binary_target()?.into(),
            declared_capabilities: artifacts.declaration.capabilities.clone(),
            artifacts: BTreeMap::from([
                (
                    LifecycleArtifactRole::Supervisor,
                    LifecycleArtifactInspection {
                        descriptor: artifacts
                            .supervisor
                            .handle
                            .inherited_descriptor()
                            .map_err(anyhow::Error::msg)?,
                        digest: artifacts.supervisor.identity.content_hash.clone(),
                        bytes: supervisor_bytes,
                    },
                ),
                (
                    LifecycleArtifactRole::Launcher,
                    LifecycleArtifactInspection {
                        descriptor: artifacts
                            .launcher
                            .handle
                            .inherited_descriptor()
                            .map_err(anyhow::Error::msg)?,
                        digest: artifacts.launcher.identity.content_hash.clone(),
                        bytes: launcher_bytes,
                    },
                ),
            ]),
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
            vec![
                artifacts.supervisor.handle.clone(),
                artifacts.launcher.handle.clone(),
            ],
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
            settings_handle.clone(),
            credential_handle.clone(),
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
            inherited.extend(guest_inputs.retained_descriptors());
        }
        drop(descriptors);
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
        let response = self.invoke(contract, credential, &request, None, None, deadline)?;
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
                guest_inputs: activation_authority
                    .context("first lifecycle activation has no guest input authority")?
                    .guest_inputs()
                    .projection()
                    .clone(),
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
            .invoke(contract, credential, &request, None, None, deadline)?
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
            artifacts: request.artifacts.clone(),
        };
        let valid = serde_json::to_value(response).unwrap();
        decode_inspection_response(&serde_json::to_vec(&valid).unwrap(), &request).unwrap();
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
