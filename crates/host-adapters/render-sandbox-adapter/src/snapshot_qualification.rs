//! Render-specific interpretation of an independently produced snapshot probe.
//!
//! Parsing and matching this shape grants no lifecycle capability. The daemon
//! must first authenticate a current, published product qualification and its
//! admitted verifier execution, then join these fields to the signed binding.

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::io::Cursor;

use ryeos_external_execution_contract::restored_runtime_measurement::{
    MAX_RESTORED_OWNER_RESULT_BYTES, MAX_RESTORED_VERIFIER_ADAPTER_REQUEST_BYTES,
    RESTORATION_VERIFIER_REMOTE_DIRECTORY, RESTORED_VERIFIER_ADAPTER_PROTOCOL,
    RestoredOwnerChallenge, RestoredOwnerMeasurement, RestoredVerifierAdapterRequest,
    RestoredVerifierAdapterResponse,
};
use ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotLocator;
use ryeos_external_execution_contract::runtime_snapshot::{
    MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES, RuntimeSnapshotIntent,
    RuntimeSnapshotQualificationAdapterRequest, RuntimeSnapshotQualificationAdapterResponse,
    RuntimeSnapshotQualificationOccurrence, RuntimeSnapshotReadinessObservation,
};
use ryeos_external_execution_contract::{
    LIFECYCLE_ADAPTER_PROTOCOL, LifecycleRuntimeProbeRequest, LifecycleRuntimeProbeResponse,
};
use ryeos_http_transport::{SseLimits, SseReader};

use crate::provider_spec::{PlanValue, ProviderSpec};
use crate::{ADAPTER_ID, RenderPlan, Settings};

const MAX_RESTORED_RUN_STREAM_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationSettings {
    schema: u32,
    owner_id: String,
    plan: RenderPlan,
    region: String,
    sandbox_group_id: String,
    tls_roots_der_base64: Vec<String>,
}

fn selected_qualification_settings(
    bytes: &[u8],
    digest: &str,
    group_id: &str,
    snapshot_id: &str,
) -> Result<Settings> {
    ensure!(
        lillux::sha256_hex(bytes) == digest,
        "qualification settings changed their signed identity"
    );
    let settings: QualificationSettings =
        ryeos_external_execution_contract::from_json_slice_strict(
            bytes,
            crate::MAX_SETTINGS_BYTES,
        )?;
    ensure!(
        ryeos_external_execution_contract::canonical_json(&serde_json::from_slice::<
            serde_json::Value,
        >(bytes)?)?
            == bytes
            && settings.schema == 1
            && settings.sandbox_group_id == group_id,
        "qualification settings are noncanonical or differ from retained group"
    );
    let selected = Settings {
        schema: 2,
        owner_id: settings.owner_id,
        plan: settings.plan,
        region: settings.region,
        snapshot_id: snapshot_id.into(),
        tls_roots_der_base64: settings.tls_roots_der_base64,
    };
    crate::validate_settings(&selected)?;
    Ok(selected)
}

/// The only create contact for the retained qualification attempt. The
/// caller must durably claim that attempt before invoking this executable.
/// A lost or ambiguous response remains uncertain; this path is never a
/// recovery operation and cannot be called to retry after journal replay.
pub(crate) fn create_restored_sandbox(
    adapter: &lillux::InheritedDescriptorAuthority,
) -> Result<()> {
    let deadline = crate::operation_deadline()?;
    ensure!(
        [
            crate::LIFECYCLE_BOOTSTRAP_FD_ENV,
            crate::LIFECYCLE_SIGNED_IMPORT_FD_ENV,
            crate::LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV,
        ]
        .iter()
        .all(|name| std::env::var_os(name).is_none()),
        "snapshot qualification create received worker activation authority"
    );
    let bytes = crate::read_sealed_env(
        crate::LIFECYCLE_REQUEST_FD_ENV,
        MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
    )?;
    let request: RuntimeSnapshotQualificationAdapterRequest =
        ryeos_external_execution_contract::from_json_slice_strict(
            &bytes,
            MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
        )?;
    request.validate()?;
    ensure!(
        ryeos_external_execution_contract::canonical_json(&request)? == bytes
            && request.intent.provider_id == ADAPTER_ID,
        "snapshot qualification request is noncanonical or selects another provider"
    );
    crate::verify_artifact(adapter, &request.intent.adapter_artifact_hash, None)?;
    let deadline = crate::request_deadline(request.intent.attempt_deadline_ms, deadline)?;
    let spec_bytes = crate::read_sealed_env(
        crate::LIFECYCLE_PROVIDER_SPEC_FD_ENV,
        usize::try_from(ryeos_external_execution_contract::MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )?;
    let spec_digest = std::env::var(crate::LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)
        .context("qualification create lacks captured provider spec digest")?;
    ensure!(
        spec_digest == request.provider_spec_digest
            && lillux::sha256_hex(&spec_bytes) == request.provider_spec_digest,
        "qualification create provider spec changed its signed handoff"
    );
    let spec = ProviderSpec::parse(
        &spec_bytes,
        &lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json")),
    )?;
    let settings_bytes =
        crate::read_sealed_env(crate::LIFECYCLE_SETTINGS_FD_ENV, crate::MAX_SETTINGS_BYTES)?;
    let selected = selected_qualification_settings(
        &settings_bytes,
        &request.intent.settings_digest,
        &request.intent.provider_group_id,
        &request.locator.snapshot_id,
    )?;
    let plan = match selected.plan {
        RenderPlan::Starter => PlanValue::Starter,
        RenderPlan::Standard => PlanValue::Standard,
        RenderPlan::Pro => PlanValue::Pro,
    };
    let projection = spec.qualification_create_projection(
        &selected.owner_id,
        plan,
        &selected.region,
        &request.locator.snapshot_id,
        request.intent.maximum_lifetime_seconds,
    )?;
    let route = spec
        .qualification_create_route()
        .context("signed provider spec has no qualification create route")?;
    let (url, target) = crate::api_url(&spec, route, None, &selected, None)?;
    crate::validate_api_url(
        &url,
        &target.path_segments,
        target.owner_id_query.as_deref(),
        None,
    )?;
    let body = crate::CreateSandboxBody {
        owner_id: projection.owner_id.clone(),
        plan: selected.plan,
        region: projection.region.clone(),
        timeout_seconds: projection.timeout_seconds,
        network_policy: crate::NetworkPolicy {
            default: crate::RenderNetworkPolicyDefault::DenyAll,
        },
        snapshot_id: projection.snapshot_id.clone(),
    };
    let body = ryeos_external_execution_contract::canonical_json(&body)?;
    let network = crate::network_context_from_captured_inputs()?;
    let cancellation = lillux::network::NetworkCancellation::default();
    let _signal_cancellation = crate::SignalCancellation::install(cancellation.clone())?;
    let credential = crate::read_credential()?;
    let result = match crate::send_api_request(
        &network,
        &url,
        "POST",
        Some(body),
        &credential,
        deadline,
        &selected,
        &cancellation,
    ) {
        Ok(response) => match crate::read_response(response) {
            Ok((status, body)) => match crate::accepted_create_response(status, &body, &projection)
            {
                Some(sandbox) => RuntimeSnapshotQualificationAdapterResponse::OccurrenceBound {
                    occurrence: RuntimeSnapshotQualificationOccurrence {
                        schema: 1,
                        operation_id: request.intent.operation_id.clone(),
                        occurrence_id: sandbox.id,
                        provider_response_sha256: lillux::sha256_hex(&body),
                        contact_deadline_exceeded: false,
                    },
                },
                None => RuntimeSnapshotQualificationAdapterResponse::Uncertain {
                    operation_id: request.intent.operation_id.clone(),
                },
            },
            Err(()) => RuntimeSnapshotQualificationAdapterResponse::Uncertain {
                operation_id: request.intent.operation_id.clone(),
            },
        },
        Err(_) => RuntimeSnapshotQualificationAdapterResponse::Uncertain {
            operation_id: request.intent.operation_id.clone(),
        },
    };
    result.validate_for(&request)?;
    crate::write_response(&result)
}

/// Invoked only after the daemon's durable verifier-attempt claim. The entire
/// sealed source, upload and route preflight precedes credential access. Any
/// ambiguous upload, token mint, run or stream is returned as uncertainty;
/// reconciliation must never invoke this entry again.
pub(crate) fn run_restored_verifier(adapter: &lillux::InheritedDescriptorAuthority) -> Result<()> {
    let enclosing = crate::operation_deadline()?;
    ensure!(
        [
            crate::LIFECYCLE_BOOTSTRAP_FD_ENV,
            crate::LIFECYCLE_SIGNED_IMPORT_FD_ENV,
            crate::LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV,
        ]
        .iter()
        .all(|name| std::env::var_os(name).is_none()),
        "restored verifier received worker activation authority"
    );
    let bytes = crate::read_sealed_env(
        crate::LIFECYCLE_REQUEST_FD_ENV,
        MAX_RESTORED_VERIFIER_ADAPTER_REQUEST_BYTES,
    )?;
    let request: RestoredVerifierAdapterRequest =
        ryeos_external_execution_contract::from_json_slice_strict(
            &bytes,
            MAX_RESTORED_VERIFIER_ADAPTER_REQUEST_BYTES,
        )?;
    request.validate()?;
    ensure!(
        request.protocol == RESTORED_VERIFIER_ADAPTER_PROTOCOL
            && ryeos_external_execution_contract::canonical_json(&request)? == bytes
            && request.source_intent.provider_id == ADAPTER_ID,
        "restored verifier request is noncanonical or selects another provider"
    );
    crate::verify_artifact(
        adapter,
        &request.qualification_intent.adapter_artifact_hash,
        None,
    )?;
    let deadline = crate::request_deadline(request.intent.attempt_deadline_ms, enclosing)?;
    let spec_bytes = crate::read_sealed_env(
        crate::LIFECYCLE_PROVIDER_SPEC_FD_ENV,
        usize::try_from(ryeos_external_execution_contract::MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )?;
    let captured_spec_digest = std::env::var(crate::LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)
        .context("restored verifier lacks captured provider spec digest")?;
    ensure!(
        captured_spec_digest == request.provider_spec_digest
            && lillux::sha256_hex(&spec_bytes) == request.provider_spec_digest,
        "restored verifier provider spec changed its signed handoff"
    );
    let spec = ProviderSpec::parse(
        &spec_bytes,
        &lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json")),
    )?;
    let settings_bytes =
        crate::read_sealed_env(crate::LIFECYCLE_SETTINGS_FD_ENV, crate::MAX_SETTINGS_BYTES)?;
    let settings = selected_qualification_settings(
        &settings_bytes,
        &request.qualification_intent.settings_digest,
        &request.qualification_intent.provider_group_id,
        &request.locator.snapshot_id,
    )?;
    // SAFETY: the trusted runner transferred this exact sealed descriptor
    // once into this single-threaded, first-claim adapter invocation.
    let upload = unsafe { lillux::take_inherited_descriptor_authority(request.upload_descriptor) }
        .map_err(anyhow::Error::msg)?;
    preflight_verifier_upload_and_routes(&request, &upload, &spec, &settings)?;
    let network = crate::network_context_from_captured_inputs()?;
    let cancellation = lillux::network::NetworkCancellation::default();
    let _signal_cancellation = crate::SignalCancellation::install(cancellation.clone())?;
    let result = crate::restored_verifier_contact::first_contact(
        &network,
        &spec,
        &settings,
        &request,
        &upload,
        deadline,
        &cancellation,
    )
    .unwrap_or_else(|_| RestoredVerifierAdapterResponse::Uncertain {
        operation_id: request.intent.operation_id.clone(),
    });
    result.validate_for(&request)?;
    crate::write_response(&result)
}

fn preflight_verifier_upload_and_routes(
    request: &RestoredVerifierAdapterRequest,
    upload: &lillux::InheritedDescriptorAuthority,
    spec: &ProviderSpec,
    settings: &Settings,
) -> Result<()> {
    request.validate()?;
    upload.require_owned_regular()?;
    let observation = upload.regular_file_observation()?;
    ensure!(
        observation.full_permission_mode()? == 0o600
            && observation.size() == request.upload_bytes
            && upload.digest_regular_file_stable_exact(&observation)? == request.upload_sha256,
        "sealed restored verifier upload differs from retained attempt"
    );
    let (upload_route, run_route) = spec.qualification_verifier_routes();
    for (route, operation, path) in [
        (
            upload_route,
            crate::proxy_route::ProxyOperation::UploadFile {
                remote_path: RESTORATION_VERIFIER_REMOTE_DIRECTORY,
            },
            Some(RESTORATION_VERIFIER_REMOTE_DIRECTORY),
        ),
        (
            run_route,
            crate::proxy_route::ProxyOperation::RunStream,
            None,
        ),
    ] {
        let (url, target) = crate::api_url(
            spec,
            route,
            Some(&request.occurrence.occurrence_id),
            settings,
            path,
        )?;
        crate::validate_api_url(
            &url,
            &target.path_segments,
            target.owner_id_query.as_deref(),
            target.upload_path_query.as_deref(),
        )?;
        ensure!(
            url == crate::proxy_route::connect_token_url(
                &request.occurrence.occurrence_id,
                &settings.owner_id,
                operation,
            )?,
            "signed verifier route differs from exact Render token operation"
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifierOutputEvent {
    stream: String,
    data: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifierExitEvent {
    exit_code: i32,
}

/// Parsed content from one complete provider run stream. This remains an
/// untrusted transport observation until the daemon binds the token/run ID,
/// exact verifier artifact and restored occurrence to its durable allocation.
pub(crate) struct RestoredVerifierStreamObservation {
    pub measurement: RestoredOwnerMeasurement,
    pub response_sha256: String,
}

pub(crate) fn parse_restored_verifier_stream(
    body: &[u8],
    challenge: &RestoredOwnerChallenge,
    intent: &RuntimeSnapshotIntent,
    locator: &RuntimeSnapshotLocator,
    readiness: &RuntimeSnapshotReadinessObservation,
) -> Result<RestoredVerifierStreamObservation> {
    ensure!(
        !body.is_empty() && body.len() <= MAX_RESTORED_RUN_STREAM_BYTES,
        "restored verifier stream exceeds its bound"
    );
    let mut events = SseReader::new(
        Cursor::new(body),
        SseLimits {
            line_bytes: 16 * 1024,
            event_bytes: 16 * 1024,
            total_bytes: MAX_RESTORED_RUN_STREAM_BYTES as u64,
            events: 32,
        },
    )?;
    let mut stdout = String::new();
    let mut exited = false;
    while let Some(event) = events.next_event()? {
        ensure!(
            event.id.is_none() && event.retry_ms.is_none() && !exited,
            "restored verifier stream has unexpected event authority"
        );
        match event.event.as_deref() {
            Some("output") => {
                let output: VerifierOutputEvent =
                    ryeos_external_execution_contract::from_json_slice_strict(
                        event.data.as_bytes(),
                        16 * 1024,
                    )?;
                ensure!(
                    output.stream == "stdout" && !output.data.is_empty(),
                    "restored verifier emitted non-measurement output"
                );
                stdout.push_str(&output.data);
                ensure!(
                    stdout.len() <= MAX_RESTORED_OWNER_RESULT_BYTES + 1,
                    "restored verifier output exceeds its bound"
                );
            }
            Some("exit") => {
                let exit: VerifierExitEvent =
                    ryeos_external_execution_contract::from_json_slice_strict(
                        event.data.as_bytes(),
                        1024,
                    )?;
                ensure!(
                    exit.exit_code == 0,
                    "restored verifier did not exit successfully"
                );
                exited = true;
            }
            _ => anyhow::bail!("restored verifier stream has unsupported event"),
        }
    }
    ensure!(exited, "restored verifier stream has no complete exit");
    let bytes = stdout
        .strip_suffix('\n')
        .context("restored verifier output lacks its single terminator")?
        .as_bytes();
    let measurement: RestoredOwnerMeasurement =
        ryeos_external_execution_contract::from_json_slice_strict(
            bytes,
            MAX_RESTORED_OWNER_RESULT_BYTES,
        )?;
    ensure!(
        ryeos_external_execution_contract::canonical_json(&measurement)? == bytes,
        "restored verifier output is noncanonical"
    );
    measurement.validate_content_for_bound_snapshot(challenge, intent, locator, readiness)?;
    Ok(RestoredVerifierStreamObservation {
        measurement,
        response_sha256: lillux::sha256_hex(body),
    })
}

pub(crate) fn interpret_authenticated_request(
    request: &LifecycleRuntimeProbeRequest,
    settings: &Settings,
) -> Result<LifecycleRuntimeProbeResponse> {
    request.validate()?;
    ensure!(
        request.adapter_id == ADAPTER_ID
            && request.source.manifest_hash
                == request
                    .probe_evidence
                    .get("guest_runtime_manifest_hash")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
        "runtime probe request differs from exact Render adapter or source"
    );
    let root_b64 = request
        .source
        .controller_public_root
        .strip_prefix("ed25519:")
        .ok_or_else(|| anyhow::anyhow!("runtime probe source has no controller public root"))?;
    let root_bytes = base64::engine::general_purpose::STANDARD.decode(root_b64)?;
    let root_bytes: [u8; 32] = root_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("runtime probe controller root has wrong length"))?;
    ensure!(
        base64::engine::general_purpose::STANDARD.encode(root_bytes) == root_b64
            && lillux::sha256_hex(hex::encode(root_bytes).as_bytes())
                == request.source.controller_root_blob_sha256,
        "runtime probe controller root differs from authenticated product"
    );
    let probe = RenderSnapshotProbe::from_probe_evidence(&request.probe_evidence)?;
    probe.validate_for(
        settings,
        &SnapshotExpectation {
            product_witness_hash: &request.product_witness_hash,
            guest_runtime_manifest_hash: &request.source.manifest_hash,
            controller_public_root: &request.source.controller_public_root,
            account: &request.account,
            binding_hash: &request.binding_hash,
            installed_owner_hash: &request.source.owner_executable_sha256,
        },
    )?;
    Ok(LifecycleRuntimeProbeResponse {
        schema: 1,
        protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
        adapter_id: ADAPTER_ID.into(),
        request_digest: request.digest()?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenderSnapshotProbe {
    pub schema: u32,
    pub product_witness_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub controller_public_root: String,
    pub owner_id: String,
    pub account: String,
    pub snapshot_id: String,
    /// The daemon independently rejoins this exact value to its bound
    /// one-attempt journal before invoking the credential-free adapter.
    pub runtime_snapshot_locator: RuntimeSnapshotLocator,
    /// CAS-authored pointer to the daemon's complete, timely verifier run.
    /// The adapter checks shape; the daemon authenticates the exact row.
    pub restored_verifier_operation_id: String,
    pub restored_verifier_observation_hash: String,
    pub snapshot_kind: String,
    pub plan: RenderPlan,
    pub region: String,
    pub binding_hash: String,
    pub restored_tree_manifest_hash: String,
    pub installed_owner_hash: String,
    pub installed_controller_public_root: String,
    pub signed_import_mode: u32,
    pub guest_package_mode: u32,
    pub lost_stream_survival_evidence_hash: String,
    pub authenticated_ready_evidence_hash: String,
    pub whole_guest_termination_evidence_hash: String,
    pub writer_exclusion_evidence_hash: String,
}

pub(crate) struct SnapshotExpectation<'a> {
    pub product_witness_hash: &'a str,
    pub guest_runtime_manifest_hash: &'a str,
    pub controller_public_root: &'a str,
    pub account: &'a str,
    pub binding_hash: &'a str,
    pub installed_owner_hash: &'a str,
}

impl RenderSnapshotProbe {
    /// Decode bounded probe data only after the caller has authenticated the
    /// published qualification and its independent verifier execution.
    pub(crate) fn from_probe_evidence(value: &serde_json::Value) -> Result<Self> {
        ensure!(
            lillux::canonical_json(value)?.len() <= 32 * 1024,
            "Render snapshot probe exceeds its byte bound"
        );
        Ok(serde_json::from_value(value.clone())?)
    }

    pub(crate) fn validate_for(
        &self,
        settings: &Settings,
        expected: &SnapshotExpectation<'_>,
    ) -> Result<()> {
        ensure!(self.schema == 3, "unsupported Render snapshot probe schema");
        for hash in [
            &self.product_witness_hash,
            &self.guest_runtime_manifest_hash,
            &self.binding_hash,
            &self.restored_tree_manifest_hash,
            &self.installed_owner_hash,
            &self.lost_stream_survival_evidence_hash,
            &self.authenticated_ready_evidence_hash,
            &self.whole_guest_termination_evidence_hash,
            &self.writer_exclusion_evidence_hash,
            &self.restored_verifier_operation_id,
            &self.restored_verifier_observation_hash,
        ] {
            ensure!(
                lillux::valid_hash(hash),
                "Render snapshot probe has an invalid content identity"
            );
        }
        let root = self
            .controller_public_root
            .strip_prefix("ed25519:")
            .ok_or_else(|| {
                anyhow::anyhow!("Render snapshot probe has no controller public root")
            })?;
        let decoded = base64::engine::general_purpose::STANDARD.decode(root)?;
        let root_bytes: [u8; 32] = decoded
            .try_into()
            .map_err(|_| anyhow::anyhow!("Render snapshot controller root has the wrong length"))?;
        let root_key = lillux::crypto::VerifyingKey::from_bytes(&root_bytes)?;
        ensure!(
            !root_key.is_weak()
                && base64::engine::general_purpose::STANDARD.encode(root_key.to_bytes()) == root,
            "Render snapshot controller root is weak or noncanonical"
        );
        ensure!(
            self.product_witness_hash == expected.product_witness_hash
                && self.guest_runtime_manifest_hash == expected.guest_runtime_manifest_hash
                && self.controller_public_root == expected.controller_public_root
                && self.owner_id == settings.owner_id
                && self.account == expected.account
                && self.snapshot_id == settings.snapshot_id
                && self.runtime_snapshot_locator.schema
                    == ryeos_external_execution_contract::runtime_snapshot::RUNTIME_SNAPSHOT_RESULT_SCHEMA
                && self.runtime_snapshot_locator.snapshot_id == self.snapshot_id
                && self.runtime_snapshot_locator.operation_id.len() == 64
                && lillux::valid_hash(&self.runtime_snapshot_locator.operation_id)
                && lillux::valid_hash(&self.runtime_snapshot_locator.intent_digest)
                && lillux::valid_hash(&self.runtime_snapshot_locator.provider_response_sha256)
                && lillux::valid_hash(&self.runtime_snapshot_locator.adapter_observation_sha256)
                && self.snapshot_kind == "filesystem"
                && self.plan == settings.plan
                && self.region == settings.region
                && self.binding_hash == expected.binding_hash
                && self.installed_owner_hash == expected.installed_owner_hash
                && self.installed_controller_public_root == expected.controller_public_root
                && self.restored_tree_manifest_hash == expected.guest_runtime_manifest_hash
                // Render's observed upload is owner-writable 0600. The
                // one-shot owner accepts that exact mode beneath its private
                // activation root; 0400 would refuse a real upload.
                && self.signed_import_mode == 0o600
                && self.guest_package_mode == 0o600,
            "Render snapshot probe differs from the exact admitted placement or restored runtime"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restored_stream_fixture() -> (
        RestoredOwnerChallenge,
        RuntimeSnapshotIntent,
        RuntimeSnapshotLocator,
        RuntimeSnapshotReadinessObservation,
        Vec<u8>,
    ) {
        let mut intent = RuntimeSnapshotIntent {
            schema: 1,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "1".repeat(64)),
            provider_id: ADAPTER_ID.into(),
            source_occurrence_id: "sbx-source".into(),
            provider_group_id: "sbg-exact".into(),
            production_profile_digest: "2".repeat(64),
            adapter_artifact_hash: "3".repeat(64),
            provider_spec_digest: "4".repeat(64),
            settings_digest: "5".repeat(64),
            product_witness_hash: "6".repeat(64),
            guest_runtime_manifest_hash: "7".repeat(64),
            owner_executable_sha256: "8".repeat(64),
            controller_public_root: public_root(),
            upload_sha256: "9".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: 42,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        let locator = RuntimeSnapshotLocator {
            schema:
                ryeos_external_execution_contract::runtime_snapshot::RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            snapshot_id: "snp-exact".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: lillux::sha256_hex(br#"{"schema":1}"#),
        };
        let readiness = RuntimeSnapshotReadinessObservation {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            creation_response_sha256: locator.provider_response_sha256.clone(),
            readiness_response_sha256: "b".repeat(64),
            captured_at: "2026-09-28T00:01:00Z".into(),
            size_bytes: 4096,
        };
        let challenge = RestoredOwnerChallenge {
            schema: 1,
            protocol: ryeos_external_execution_contract::restored_runtime_measurement::RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            operation_id: intent.operation_id.clone(),
            snapshot_id: locator.snapshot_id.clone(),
            restored_occurrence_id: "sbx-restored".into(),
            nonce_hex: "c".repeat(64),
        };
        let measurement = RestoredOwnerMeasurement {
            schema: 1,
            protocol: challenge.protocol.clone(),
            challenge_digest: challenge.digest().unwrap(),
            manifest_hash: intent.guest_runtime_manifest_hash.clone(),
            owner_executable_sha256: intent.owner_executable_sha256.clone(),
            controller_public_root: intent.controller_public_root.clone(),
        };
        let output = format!(
            "{}\n",
            String::from_utf8(
                ryeos_external_execution_contract::canonical_json(&measurement).unwrap()
            )
            .unwrap()
        );
        let event = serde_json::json!({"stream":"stdout","data":output});
        let stream =
            format!("event: output\ndata: {event}\n\nevent: exit\ndata: {{\"exit_code\":0}}\n\n")
                .into_bytes();
        (challenge, intent, locator, readiness, stream)
    }

    #[test]
    fn restored_verifier_stream_requires_exact_content_and_complete_zero_exit() {
        let (challenge, intent, locator, readiness, stream) = restored_stream_fixture();
        let observed =
            parse_restored_verifier_stream(&stream, &challenge, &intent, &locator, &readiness)
                .unwrap();
        assert_eq!(
            observed.measurement.manifest_hash,
            intent.guest_runtime_manifest_hash
        );
        assert_eq!(observed.response_sha256, lillux::sha256_hex(&stream));
        let no_exit = String::from_utf8(stream.clone()).unwrap();
        let no_exit = &no_exit[..no_exit.find("event: exit").unwrap()];
        assert!(
            parse_restored_verifier_stream(
                no_exit.as_bytes(),
                &challenge,
                &intent,
                &locator,
                &readiness
            )
            .is_err()
        );
        let nonzero = String::from_utf8(stream.clone())
            .unwrap()
            .replace("\"exit_code\":0", "\"exit_code\":1");
        assert!(
            parse_restored_verifier_stream(
                nonzero.as_bytes(),
                &challenge,
                &intent,
                &locator,
                &readiness
            )
            .is_err()
        );
        let extra = format!(
            "{}event: output\ndata: {{\"stream\":\"stdout\",\"data\":\"extra\"}}\n\n",
            String::from_utf8(stream).unwrap()
        );
        assert!(
            parse_restored_verifier_stream(
                extra.as_bytes(),
                &challenge,
                &intent,
                &locator,
                &readiness
            )
            .is_err()
        );
        let mut wrong = challenge;
        wrong.nonce_hex = "d".repeat(64);
        let (_, _, _, _, stream) = restored_stream_fixture();
        assert!(
            parse_restored_verifier_stream(&stream, &wrong, &intent, &locator, &readiness).is_err()
        );
    }

    #[test]
    fn sealed_verifier_handoff_and_observation_join_exact_occurrence() {
        use ryeos_external_execution_contract::restored_runtime_measurement::{
            RESTORED_VERIFIER_ADAPTER_PROTOCOL, RestoredVerifierAdapterObservation,
            RestoredVerifierAdapterRequest, RestoredVerifierAdapterResponse,
            RestoredVerifierAttemptIntent,
        };
        use ryeos_external_execution_contract::runtime_snapshot::{
            RuntimeSnapshotQualificationIntent, RuntimeSnapshotQualificationOccurrence,
        };

        let (challenge, source_intent, locator, readiness, stream) = restored_stream_fixture();
        let upload = lillux::sealed_memfd(c"verifier-upload-test", b"tar-fixture").unwrap();
        let upload_sha256 = lillux::sha256_hex(b"tar-fixture");
        let mut qualification_intent = RuntimeSnapshotQualificationIntent {
            schema: 1,
            operation_id: String::new(),
            owner_principal: source_intent.owner_principal.clone(),
            snapshot_operation_id: source_intent.operation_id.clone(),
            snapshot_intent_digest: source_intent.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            provider_id: source_intent.provider_id.clone(),
            provider_group_id: source_intent.provider_group_id.clone(),
            qualification_profile_digest: "d".repeat(64),
            adapter_artifact_hash: "e".repeat(64),
            provider_spec_digest: "f".repeat(64),
            settings_digest: "1".repeat(64),
            verifier_artifact_hash: "2".repeat(64),
            maximum_lifetime_seconds: 900,
            attempt_deadline_ms: 42,
        };
        qualification_intent.operation_id = qualification_intent.derived_operation_id().unwrap();
        let occurrence = RuntimeSnapshotQualificationOccurrence {
            schema: 1,
            operation_id: qualification_intent.operation_id.clone(),
            occurrence_id: challenge.restored_occurrence_id.clone(),
            provider_response_sha256: "3".repeat(64),
            contact_deadline_exceeded: false,
        };
        let mut attempt = RestoredVerifierAttemptIntent {
            schema: 1,
            operation_id: String::new(),
            qualification_operation_id: qualification_intent.operation_id.clone(),
            restored_occurrence_id: occurrence.occurrence_id.clone(),
            verifier_artifact_hash: qualification_intent.verifier_artifact_hash.clone(),
            upload_sha256: upload_sha256.clone(),
            upload_bytes: 11,
            challenge,
            attempt_deadline_ms: 42,
        };
        attempt.operation_id = attempt.derived_operation_id().unwrap();
        let request = RestoredVerifierAdapterRequest {
            protocol: RESTORED_VERIFIER_ADAPTER_PROTOCOL.into(),
            provider_spec_digest: qualification_intent.provider_spec_digest.clone(),
            intent: attempt,
            source_intent,
            locator,
            readiness,
            qualification_intent,
            occurrence,
            upload_descriptor: 4,
            upload_bytes: 11,
            upload_sha256,
        };
        request.validate().unwrap();
        let spec = ProviderSpec::parse(
            include_bytes!("../fixtures/provider-spec.json"),
            &lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json")),
        )
        .unwrap();
        let settings = Settings {
            schema: 2,
            owner_id: "owner-1".into(),
            plan: RenderPlan::Starter,
            region: "oregon".into(),
            snapshot_id: request.locator.snapshot_id.clone(),
            tls_roots_der_base64: vec![],
        };
        preflight_verifier_upload_and_routes(&request, &upload, &spec, &settings).unwrap();
        let parsed = parse_restored_verifier_stream(
            &stream,
            &request.intent.challenge,
            &request.source_intent,
            &request.locator,
            &request.readiness,
        )
        .unwrap();
        let response = RestoredVerifierAdapterResponse::Observed {
            observation: RestoredVerifierAdapterObservation {
                schema: 1,
                operation_id: request.intent.operation_id.clone(),
                occurrence_id: request.occurrence.occurrence_id.clone(),
                upload_token_execution_id: "exe-upload".into(),
                run_token_execution_id: "exe-run".into(),
                upload_response_sha256: "5".repeat(64),
                run_stream_sha256: parsed.response_sha256,
                measurement: parsed.measurement,
                contact_deadline_exceeded: false,
            },
        };
        response.validate_for(&request).unwrap();
        let mut changed = request.clone();
        changed.occurrence.occurrence_id = "sbx-other".into();
        assert!(response.validate_for(&changed).is_err());
        let mut changed = request;
        changed.upload_sha256 = "6".repeat(64);
        assert!(response.validate_for(&changed).is_err());
        assert!(preflight_verifier_upload_and_routes(&changed, &upload, &spec, &settings).is_err());
    }

    fn public_root() -> String {
        let signing = lillux::crypto::SigningKey::from_bytes(&[3; 32]);
        format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().to_bytes())
        )
    }

    fn probe() -> RenderSnapshotProbe {
        RenderSnapshotProbe {
            schema: 3,
            product_witness_hash: "1".repeat(64),
            guest_runtime_manifest_hash: "2".repeat(64),
            controller_public_root: public_root(),
            owner_id: "owner".into(),
            account: "account".into(),
            snapshot_id: "snp-exact".into(),
            runtime_snapshot_locator: RuntimeSnapshotLocator {
                schema: ryeos_external_execution_contract::runtime_snapshot::RUNTIME_SNAPSHOT_RESULT_SCHEMA,
                operation_id: "a".repeat(64),
                intent_digest: "b".repeat(64),
                source_occurrence_id: "sbx-source".into(),
                provider_group_id: "sbg-exact".into(),
                snapshot_id: "snp-exact".into(),
                provider_response_sha256: "c".repeat(64),
                provider_creation_observation: serde_json::json!({"schema": 1}),
                adapter_observation_sha256: lillux::sha256_hex(br#"{"schema":1}"#),
            },
            restored_verifier_operation_id: "d".repeat(64),
            restored_verifier_observation_hash: "e".repeat(64),
            snapshot_kind: "filesystem".into(),
            plan: RenderPlan::Starter,
            region: "oregon".into(),
            binding_hash: "4".repeat(64),
            restored_tree_manifest_hash: "2".repeat(64),
            installed_owner_hash: "5".repeat(64),
            installed_controller_public_root: public_root(),
            signed_import_mode: 0o600,
            guest_package_mode: 0o600,
            lost_stream_survival_evidence_hash: "6".repeat(64),
            authenticated_ready_evidence_hash: "7".repeat(64),
            whole_guest_termination_evidence_hash: "8".repeat(64),
            writer_exclusion_evidence_hash: "9".repeat(64),
        }
    }

    #[test]
    fn probe_requires_all_exact_placement_and_execution_coordinates() {
        let settings = Settings {
            schema: 2,
            owner_id: "owner".into(),
            plan: RenderPlan::Starter,
            region: "oregon".into(),
            snapshot_id: "snp-exact".into(),
            tls_roots_der_base64: Vec::new(),
        };
        let expected = SnapshotExpectation {
            product_witness_hash: &"1".repeat(64),
            guest_runtime_manifest_hash: &"2".repeat(64),
            controller_public_root: &public_root(),
            account: "account",
            binding_hash: &"4".repeat(64),
            installed_owner_hash: &"5".repeat(64),
        };
        let mut observed = probe();
        observed.validate_for(&settings, &expected).unwrap();
        observed.snapshot_id = "snp-other".into();
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.runtime_snapshot_locator.snapshot_id = "snp-other".into();
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.runtime_snapshot_locator.provider_response_sha256 = "invalid".into();
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.snapshot_kind = "runtime".into();
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.installed_controller_public_root = "ed25519:other".into();
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.signed_import_mode = 0o400;
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.signed_import_mode = 0o644;
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.guest_package_mode = 0o644;
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.whole_guest_termination_evidence_hash.clear();
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.restored_verifier_operation_id.clear();
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.restored_verifier_observation_hash.clear();
        assert!(observed.validate_for(&settings, &expected).is_err());

        let mut untrusted = serde_json::to_value(probe()).unwrap();
        untrusted
            .as_object_mut()
            .unwrap()
            .remove("runtime_snapshot_locator");
        assert!(RenderSnapshotProbe::from_probe_evidence(&untrusted).is_err());
        untrusted = serde_json::to_value(probe()).unwrap();
        untrusted
            .as_object_mut()
            .unwrap()
            .remove("restored_verifier_operation_id");
        assert!(RenderSnapshotProbe::from_probe_evidence(&untrusted).is_err());
        untrusted = serde_json::to_value(probe()).unwrap();
        untrusted["qualified"] = serde_json::Value::Bool(true);
        assert!(RenderSnapshotProbe::from_probe_evidence(&untrusted).is_err());
        untrusted = serde_json::to_value(probe()).unwrap();
        untrusted["region"] = serde_json::Value::String("x".repeat(32 * 1024));
        assert!(RenderSnapshotProbe::from_probe_evidence(&untrusted).is_err());
    }

    #[test]
    fn offline_probe_interpretation_joins_protected_source_and_settings() {
        let public_root = public_root();
        let root_bytes = base64::engine::general_purpose::STANDARD
            .decode(public_root.strip_prefix("ed25519:").unwrap())
            .unwrap();
        let settings = Settings {
            schema: 2,
            owner_id: "owner".into(),
            plan: RenderPlan::Starter,
            region: "oregon".into(),
            snapshot_id: "snp-exact".into(),
            tls_roots_der_base64: Vec::new(),
        };
        let request = LifecycleRuntimeProbeRequest {
            schema: 1,
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
            adapter_id: ADAPTER_ID.into(),
            adapter_artifact_hash: "a".repeat(64),
            settings_digest: "b".repeat(64),
            binding_hash: "4".repeat(64),
            qualification_attestation_hash: "c".repeat(64),
            product_witness_hash: "1".repeat(64),
            account: "account".into(),
            source: ryeos_external_execution_contract::LifecycleRuntimeProbeSource {
                manifest_hash: "2".repeat(64),
                owner_executable_sha256: "5".repeat(64),
                controller_root_blob_sha256: lillux::sha256_hex(hex::encode(root_bytes).as_bytes()),
                controller_public_root: public_root,
            },
            probe_evidence: serde_json::to_value(probe()).unwrap(),
        };
        let response = interpret_authenticated_request(&request, &settings).unwrap();
        response.validate_for(&request).unwrap();
        let mut changed = request.clone();
        changed.source.owner_executable_sha256 = "d".repeat(64);
        assert!(interpret_authenticated_request(&changed, &settings).is_err());
        changed = request.clone();
        changed.source.controller_root_blob_sha256 = "e".repeat(64);
        assert!(interpret_authenticated_request(&changed, &settings).is_err());
        let mut changed_settings = settings;
        changed_settings.snapshot_id = "snp-other".into();
        assert!(interpret_authenticated_request(&request, &changed_settings).is_err());
    }
}
