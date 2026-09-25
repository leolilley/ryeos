//! Fail-closed controller-side adapter for the pinned early-access Render CLI
//! Sandbox schema. The Sandbox proxy is deliberately not invoked: its URL
//! validation contract and RyeOS bootstrap mapping are not established.

mod provider_spec;
mod proxy_route;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::thread::JoinHandle;

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use chrono::DateTime;
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::{Duration, MonotonicDeadline};
use provider_spec::{PlanValue, ProviderSpec, RouteName, RouteTarget};
use ryeos_external_execution_contract::{
    LIFECYCLE_ADAPTER_EXECUTABLE_FD_ENV, LIFECYCLE_ADAPTER_PROTOCOL, LIFECYCLE_CREDENTIAL_FD_ENV,
    LIFECYCLE_HOSTS_FD_ENV, LIFECYCLE_HOSTS_SHA256_ENV, LIFECYCLE_NETWORK_POLICY_SHA256_ENV,
    LIFECYCLE_PROVIDER_SPEC_FD_ENV, LIFECYCLE_PROVIDER_SPEC_SHA256_ENV,
    LIFECYCLE_REMAINING_TIMEOUT_MS_ENV, LIFECYCLE_REQUEST_FD_ENV, LIFECYCLE_RESOLVER_FD_ENV,
    LIFECYCLE_RESOLVER_SHA256_ENV, LIFECYCLE_SETTINGS_FD_ENV, LifecycleAdapterInspectionRequest,
    LifecycleAdapterInspectionResponse, LifecycleAdapterRequest, LifecycleAdapterResponse,
    LifecycleArtifactInspection, LifecycleArtifactRole, MAX_LIFECYCLE_PROVIDER_SPEC_BYTES,
    MAX_LIFECYCLE_REQUEST_BYTES, MAX_LIFECYCLE_RESPONSE_BYTES, canonical_json,
    from_json_slice_strict,
};
use ryeos_http_transport::{
    ContactState, Deadlines, Header, HttpClient, HttpError, HttpRequest, HttpResponse, Limits,
    RequestBodySource,
};
use serde::{Deserialize, Serialize};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};
use zeroize::Zeroizing;

const ADAPTER_ID: &str = "render-sandbox-early-access";
const ADAPTER_BUILD: &str = "ryeos-render-sandbox-lifecycle-adapter.0.1.0";
const MAX_SETTINGS_BYTES: usize = 256 * 1024;
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const MAX_API_RESPONSE_BYTES: u64 = 256 * 1024;
const MAX_ROOTS: usize = 8;
const MAX_ROOT_TOTAL_BYTES: usize = 256 * 1024;
const MAX_ROOT_BYTES: usize = 64 * 1024;
const MAX_NETWORK_INPUT_BYTES: usize = 64 * 1024;
const MAX_OPERATION_TIMEOUT_MS: u64 = 60 * 60 * 1000;
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    schema: u32,
    owner_id: String,
    plan: RenderPlan,
    region: String,
    snapshot_id: String,
    base_snapshot_hash: String,
    tls_roots_der_base64: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum RenderPlan {
    Starter,
    Standard,
    Pro,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CreateSandboxBody {
    #[serde(rename = "ownerId")]
    owner_id: String,
    plan: RenderPlan,
    region: String,
    #[serde(rename = "timeoutSeconds")]
    timeout_seconds: u32,
    #[serde(rename = "networkPolicy")]
    network_policy: NetworkPolicy,
    #[serde(rename = "snapshotId")]
    snapshot_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NetworkPolicy {
    default: RenderNetworkPolicyDefault,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RenderSandboxStatus {
    Creating,
    Errored,
    Resuming,
    Running,
    Suspended,
    Terminated,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum RenderSandboxPlan {
    Starter,
    Standard,
    Pro,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum RenderNetworkPolicyDefault {
    AllowAll,
    DenyAll,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RenderNetworkPolicy {
    default: RenderNetworkPolicyDefault,
}

/// Exact fields emitted by the pinned CLI's generated Sandbox model.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RenderSandbox {
    #[serde(rename = "createdAt")]
    created_at: String,
    id: String,
    #[serde(rename = "networkPolicy")]
    network_policy: RenderNetworkPolicy,
    plan: RenderSandboxPlan,
    region: String,
    status: RenderSandboxStatus,
    #[serde(rename = "terminatedAt", default)]
    terminated_at: Option<String>,
    #[serde(rename = "timeoutSeconds")]
    timeout_seconds: u32,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct AllocationEvidence<'a> {
    schema: u32,
    provider: &'static str,
    operation_id: &'a str,
    binding_hash: &'a str,
    request_digest: &'a str,
    snapshot_id: &'a str,
    base_snapshot_hash: &'a str,
    sandbox_id: &'a str,
    response_sha256: &'a str,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct TerminalEvidence<'a> {
    schema: u32,
    provider: &'static str,
    terminal_state: &'static str,
    operation_id: &'a str,
    binding_hash: &'a str,
    request_digest: &'a str,
    occurrence_id: &'a str,
    termination_request_digest: &'a str,
    terminated_at: &'a str,
    sandbox_response_sha256: &'a str,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct NoOccurrenceEvidence<'a> {
    schema: u32,
    provider: &'static str,
    operation_id: &'a str,
    binding_hash: &'a str,
    request_digest: &'a str,
    snapshot_id: &'a str,
    base_snapshot_hash: &'a str,
    basis: &'static str,
}

struct SignalCancellation {
    handle: SignalHandle,
    listener: Option<JoinHandle<()>>,
}

impl SignalCancellation {
    fn install(cancellation: NetworkCancellation) -> Result<Self> {
        let mut signals = Signals::new([SIGINT, SIGTERM])?;
        let handle = signals.handle();
        let listener = std::thread::Builder::new()
            .name("render-adapter-cancel".into())
            .spawn(move || {
                if signals.forever().next().is_some() {
                    cancellation.cancel();
                }
            })?;
        Ok(Self {
            handle,
            listener: Some(listener),
        })
    }
}

impl Drop for SignalCancellation {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

fn main() {
    if run().is_err() {
        // Never forward provider text, credentials, URLs, or settings to stderr.
        let _ = writeln!(std::io::stderr(), "render lifecycle adapter failed closed");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let adapter = unsafe {
        lillux::take_inherited_descriptor_authority_from_env(LIFECYCLE_ADAPTER_EXECUTABLE_FD_ENV)
    }
    .map_err(anyhow::Error::msg)?;
    match std::env::args().nth(1).as_deref() {
        Some("inspect") => inspect(&adapter),
        Some("operate") => operate(),
        _ => anyhow::bail!("unsupported lifecycle invocation"),
    }
}

fn inspect(adapter: &lillux::InheritedDescriptorAuthority) -> Result<()> {
    let request_bytes = read_sealed_env(LIFECYCLE_REQUEST_FD_ENV, MAX_LIFECYCLE_REQUEST_BYTES)?;
    let request: LifecycleAdapterInspectionRequest =
        from_json_slice_strict(&request_bytes, MAX_LIFECYCLE_REQUEST_BYTES)?;
    request.validate()?;
    ensure!(
        canonical_json(&request)? == request_bytes,
        "inspection request is noncanonical"
    );
    ensure!(
        request.adapter_id == ADAPTER_ID
            && request.protocol == LIFECYCLE_ADAPTER_PROTOCOL
            && request.target == lillux::platform::current_binary_target()?,
        "inspection does not match this adapter's narrow declaration"
    );
    let schema_digest = lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json"));
    ensure!(
        request.settings_schema_digest == schema_digest,
        "settings schema digest does not match this adapter"
    );
    let provider_spec_authority =
        unsafe { lillux::take_inherited_descriptor_authority(request.provider_spec.descriptor) }
            .map_err(anyhow::Error::msg)?;
    let provider_spec_descriptor = provider_spec_authority
        .inherited_descriptor()
        .map_err(anyhow::Error::msg)?;
    let provider_spec_bytes = lillux::read_sealed_inherited_descriptor(
        provider_spec_descriptor,
        usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )
    .map_err(anyhow::Error::msg)?;
    ensure!(
        u64::try_from(provider_spec_bytes.len())? == request.provider_spec.bytes,
        "provider spec size differs from the inspection declaration"
    );
    let (provider_spec, provider_spec_sha256) = parse_provider_spec_bytes(
        &provider_spec_bytes,
        &request.provider_spec.digest,
        &schema_digest,
    )?;
    let effective_capabilities = provider_spec.effective_capabilities();
    ensure!(
        !effective_capabilities.is_empty()
            && effective_capabilities.is_subset(&request.declared_capabilities),
        "provider spec proof profiles exceed the signed capability ceiling"
    );
    verify_artifact(adapter, &request.adapter_artifact_hash, None)?;

    let mut artifacts = BTreeMap::new();
    for (role, inspection) in &request.artifacts {
        // SAFETY: the trusted runner transferred each exact artifact descriptor
        // once for this single-threaded inspection process.
        let authority =
            unsafe { lillux::take_inherited_descriptor_authority(inspection.descriptor) }
                .map_err(anyhow::Error::msg)?;
        verify_artifact(&authority, &inspection.digest, Some(inspection.bytes))?;
        artifacts.insert(
            *role,
            LifecycleArtifactInspection {
                descriptor: inspection.descriptor,
                digest: inspection.digest.clone(),
                bytes: inspection.bytes,
            },
        );
    }
    ensure!(
        artifacts.len() == 2
            && artifacts.contains_key(&LifecycleArtifactRole::Supervisor)
            && artifacts.contains_key(&LifecycleArtifactRole::Launcher),
        "inspection requires exact supervisor and launcher artifacts"
    );
    let response = LifecycleAdapterInspectionResponse {
        schema: 1,
        protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
        adapter_id: ADAPTER_ID.into(),
        adapter_build: ADAPTER_BUILD.into(),
        observed_adapter_artifact_hash: request.adapter_artifact_hash.clone(),
        observed_settings_schema_digest: schema_digest,
        target: request.target.clone(),
        effective_capabilities,
        observed_provider_spec_sha256: provider_spec_sha256,
        artifacts,
    };
    response.validate_for(&request)?;
    write_response(&response)
}

fn operate() -> Result<()> {
    // Start the one operation deadline before reading any request/configuration
    // descriptor. The signed parent budget is never renewed by local work.
    let mut deadline = operation_deadline()?;
    let cancellation = NetworkCancellation::default();
    let _signal_cancellation = SignalCancellation::install(cancellation.clone())?;
    let request_bytes = read_sealed_env(LIFECYCLE_REQUEST_FD_ENV, MAX_LIFECYCLE_REQUEST_BYTES)?;
    let request: LifecycleAdapterRequest =
        from_json_slice_strict(&request_bytes, MAX_LIFECYCLE_REQUEST_BYTES)?;
    request.validate()?;
    ensure!(
        request.canonical_bytes()? == request_bytes,
        "operation request is noncanonical"
    );
    if let LifecycleAdapterRequest::Allocate { reservation, .. } = &request {
        deadline = request_deadline(reservation.contact_deadline_ms, deadline)?;
    }
    let maximum_provider_spec_bytes = usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?;
    let provider_spec_bytes =
        read_sealed_env(LIFECYCLE_PROVIDER_SPEC_FD_ENV, maximum_provider_spec_bytes)?;
    let provider_spec_sha256 = std::env::var(LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)
        .context("missing captured provider spec digest")?;
    let schema_digest = lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json"));
    let (provider_spec, _provider_spec_sha256) =
        parse_provider_spec_bytes(&provider_spec_bytes, &provider_spec_sha256, &schema_digest)?;
    let settings_bytes = read_sealed_env(LIFECYCLE_SETTINGS_FD_ENV, MAX_SETTINGS_BYTES)?;
    ensure!(
        lillux::sha256_hex(&settings_bytes) == request.common().settings_digest,
        "settings digest changed"
    );
    let settings_value: serde_json::Value =
        from_json_slice_strict(&settings_bytes, MAX_SETTINGS_BYTES)?;
    ensure!(
        lillux::canonical_json(&settings_value)?.as_bytes() == settings_bytes,
        "settings are noncanonical"
    );
    let settings: Settings = serde_json::from_value(settings_value)?;
    validate_settings(&settings)?;
    let network = network_context_from_captured_inputs()?;

    let response = match &request {
        LifecycleAdapterRequest::Allocate {
            common,
            reservation,
        } => allocate(
            common,
            reservation,
            &settings,
            &provider_spec,
            &network,
            deadline,
            &cancellation,
        ),
        LifecycleAdapterRequest::ReconcileAllocation {
            common,
            reservation,
        } => LifecycleAdapterResponse::AllocationPending {
            operation_id: common.operation_id.clone(),
            request_digest: reservation.request_digest.clone(),
        },
        LifecycleAdapterRequest::ActivateSupervisor {
            common, activation, ..
        }
        | LifecycleAdapterRequest::ReconcileSupervisorActivation {
            common, activation, ..
        } => LifecycleAdapterResponse::SupervisorPending {
            operation_id: common.operation_id.clone(),
            activation_request_digest: activation.activation_request_digest.clone(),
        },
        LifecycleAdapterRequest::Terminate {
            common,
            occurrence,
            termination,
        } => terminate(
            common,
            occurrence,
            termination,
            &settings,
            &provider_spec,
            &network,
            deadline,
            &cancellation,
        ),
        LifecycleAdapterRequest::ReconcileTermination {
            common,
            occurrence,
            termination,
        } => observe_termination(
            common,
            occurrence,
            termination,
            &settings,
            &provider_spec,
            &network,
            deadline,
            &cancellation,
        ),
    };
    response.validate_for(&request)?;
    write_response(&response)
}

fn parse_provider_spec_bytes(
    bytes: &[u8],
    declared_digest: &str,
    settings_schema_digest: &str,
) -> Result<(ProviderSpec, String)> {
    let maximum = usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= maximum,
        "provider spec is empty or exceeds its byte bound"
    );
    let observed_digest = lillux::sha256_hex(bytes);
    ensure!(
        lillux::valid_hash(declared_digest) && observed_digest == declared_digest,
        "provider spec differs from its signed digest"
    );
    let spec = ProviderSpec::parse(bytes, settings_schema_digest)?;
    Ok((spec, observed_digest))
}

fn allocate(
    common: &ryeos_external_execution_contract::LifecycleOperationCommon,
    reservation: &ryeos_external_execution_contract::AllocationReservation,
    settings: &Settings,
    provider_spec: &ProviderSpec,
    network: &NetworkContext,
    deadline: MonotonicDeadline,
    cancellation: &NetworkCancellation,
) -> LifecycleAdapterResponse {
    let pending = || LifecycleAdapterResponse::AllocationPending {
        operation_id: common.operation_id.clone(),
        request_digest: reservation.request_digest.clone(),
    };
    if !provider_spec.allocation_requires_snapshot_match()
        || reservation.base_snapshot_hash != settings.base_snapshot_hash
    {
        return pending();
    }
    let Some(route) = provider_spec.allocation_route() else {
        return pending();
    };
    let Ok((url, route_target)) = api_url(provider_spec, route, None, settings) else {
        return pending();
    };
    if validate_api_url(
        &url,
        &route_target.path_segments,
        route_target.owner_id_query.as_deref(),
    )
    .is_err()
    {
        return pending();
    }
    if !provider_spec.allocation_bind_proof_enabled() {
        return pending();
    }
    let settings_plan = match settings.plan {
        RenderPlan::Starter => PlanValue::Starter,
        RenderPlan::Standard => PlanValue::Standard,
        RenderPlan::Pro => PlanValue::Pro,
    };
    let projection = match provider_spec.create_projection(
        &settings.owner_id,
        settings_plan,
        &settings.region,
        &settings.snapshot_id,
        reservation.maximum_lifetime_seconds,
    ) {
        Ok(value) => value,
        Err(_) => return pending(),
    };
    let body = CreateSandboxBody {
        owner_id: projection.owner_id.clone(),
        plan: match projection.plan {
            PlanValue::Starter => RenderPlan::Starter,
            PlanValue::Standard => RenderPlan::Standard,
            PlanValue::Pro => RenderPlan::Pro,
        },
        region: projection.region.clone(),
        timeout_seconds: projection.timeout_seconds,
        network_policy: NetworkPolicy {
            default: match projection.network_policy_default {
                provider_spec::NetworkPolicyDefault::DenyAll => RenderNetworkPolicyDefault::DenyAll,
            },
        },
        snapshot_id: projection.snapshot_id.clone(),
    };
    let Ok(body) = canonical_json(&body) else {
        return pending();
    };
    let Ok(credential) = read_credential() else {
        return pending();
    };
    let response = match send_api_request(
        network,
        &url,
        "POST",
        Some(body),
        &credential,
        deadline,
        settings,
        cancellation,
    ) {
        Ok(response) => response,
        Err(error) => {
            if error
                .downcast_ref::<HttpError>()
                .is_some_and(|error| error.contact_state() == ContactState::NoRequestSent)
            {
                if provider_spec.allocation_no_occurrence_proof_enabled() {
                    return allocation_no_occurrence(
                        common,
                        reservation,
                        settings,
                    );
                }
            }
            // The POST may have committed. Do not retry or search the broad list.
            return pending();
        }
    };
    let (status, response_body) = match read_response(response) {
        Ok(value) => value,
        Err(()) => return pending(),
    };
    let Some(sandbox) = accepted_create_response(status, &response_body, &projection) else {
        return pending();
    };
    let response_sha256 = lillux::sha256_hex(&response_body);
    let evidence = AllocationEvidence {
        schema: 1,
        provider: "render-sandbox-early-access",
        operation_id: &common.operation_id,
        binding_hash: &common.binding_hash,
        request_digest: &reservation.request_digest,
        snapshot_id: &projection.snapshot_id,
        base_snapshot_hash: &reservation.base_snapshot_hash,
        sandbox_id: &sandbox.id,
        response_sha256: &response_sha256,
    };
    let Ok(evidence_bytes) = canonical_json(&evidence) else {
        return pending();
    };
    LifecycleAdapterResponse::AllocationBound {
        operation_id: common.operation_id.clone(),
        request_digest: reservation.request_digest.clone(),
        occurrence_id: sandbox.id,
        provider_observation_digest: lillux::sha256_hex(&evidence_bytes),
    }
}

fn allocation_no_occurrence(
    common: &ryeos_external_execution_contract::LifecycleOperationCommon,
    reservation: &ryeos_external_execution_contract::AllocationReservation,
    settings: &Settings,
) -> LifecycleAdapterResponse {
    let pending = || LifecycleAdapterResponse::AllocationPending {
        operation_id: common.operation_id.clone(),
        request_digest: reservation.request_digest.clone(),
    };
    let evidence = NoOccurrenceEvidence {
        schema: 1,
        provider: "render-sandbox-early-access",
        operation_id: &common.operation_id,
        binding_hash: &common.binding_hash,
        request_digest: &reservation.request_digest,
        snapshot_id: &settings.snapshot_id,
        base_snapshot_hash: &reservation.base_snapshot_hash,
        basis: "transport_no_request_sent",
    };
    let Ok(evidence_bytes) = canonical_json(&evidence) else {
        return pending();
    };
    LifecycleAdapterResponse::AllocationNoOccurrence {
        operation_id: common.operation_id.clone(),
        request_digest: reservation.request_digest.clone(),
        provider_observation_digest: lillux::sha256_hex(&evidence_bytes),
    }
}

fn accepted_create_response(
    status: u16,
    response_body: &[u8],
    projection: &provider_spec::CreateProjection,
) -> Option<RenderSandbox> {
    if status != 201 {
        return None;
    }
    let sandbox: RenderSandbox = from_json_slice_strict(
        response_body,
        usize::try_from(MAX_API_RESPONSE_BYTES).unwrap_or(usize::MAX),
    )
    .ok()?;
    create_response_matches(&sandbox, projection).then_some(sandbox)
}

fn create_response_matches(
    sandbox: &RenderSandbox,
    projection: &provider_spec::CreateProjection,
) -> bool {
    let expected_plan = match projection.plan {
        PlanValue::Starter => RenderSandboxPlan::Starter,
        PlanValue::Standard => RenderSandboxPlan::Standard,
        PlanValue::Pro => RenderSandboxPlan::Pro,
    };
    valid_sandbox_id(&sandbox.id)
        && DateTime::parse_from_rfc3339(&sandbox.created_at).is_ok()
        && sandbox.plan == expected_plan
        && sandbox.network_policy.default == RenderNetworkPolicyDefault::DenyAll
        && sandbox.timeout_seconds == projection.timeout_seconds
        && sandbox.region == projection.region
}

fn exact_terminal_timestamp<'a>(
    sandbox: &'a RenderSandbox,
    occurrence_id: &str,
) -> Option<&'a str> {
    if sandbox.id != occurrence_id
        || sandbox.status != RenderSandboxStatus::Terminated
        || DateTime::parse_from_rfc3339(&sandbox.created_at).is_err()
    {
        return None;
    }
    let terminated_at = sandbox.terminated_at.as_deref()?;
    DateTime::parse_from_rfc3339(terminated_at).ok()?;
    Some(terminated_at)
}

fn has_json_content_type(headers: &[Header]) -> bool {
    let mut values = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("content-type"));
    let Some(header) = values.next() else {
        return false;
    };
    if values.next().is_some()
        || !header
            .value()
            .iter()
            .all(|byte| *byte == b'\t' || (0x20..=0x7e).contains(byte))
    {
        return false;
    }
    std::str::from_utf8(header.value()).is_ok_and(is_json_content_type_value)
}

fn is_json_content_type_value(value: &str) -> bool {
    let mut parts = value.split(';');
    if !parts.next().is_some_and(|media_type| {
        trim_http_ows(media_type).eq_ignore_ascii_case("application/json")
    }) {
        return false;
    }
    let Some(parameter) = parts.next() else {
        return true;
    };
    if parts.next().is_some() {
        return false;
    }
    let Some((name, value)) = trim_http_ows(parameter).split_once('=') else {
        return false;
    };
    let value = trim_http_ows(value);
    trim_http_ows(name).eq_ignore_ascii_case("charset")
        && (value.eq_ignore_ascii_case("utf-8") || value.eq_ignore_ascii_case("\"utf-8\""))
}

fn trim_http_ows(value: &str) -> &str {
    value.trim_matches(|character| matches!(character, ' ' | '\t'))
}

fn terminate(
    common: &ryeos_external_execution_contract::LifecycleOperationCommon,
    occurrence: &ryeos_external_execution_contract::BoundOccurrence,
    termination: &ryeos_external_execution_contract::TerminationIntent,
    settings: &Settings,
    provider_spec: &ProviderSpec,
    network: &NetworkContext,
    deadline: MonotonicDeadline,
    cancellation: &NetworkCancellation,
) -> LifecycleAdapterResponse {
    let pending = || LifecycleAdapterResponse::TerminationPending {
        operation_id: common.operation_id.clone(),
        termination_request_digest: termination.termination_request_digest.clone(),
    };
    if !provider_spec.termination_terminal_proof_enabled() {
        return pending();
    }
    let Some(route) = provider_spec.termination_mutation_route() else {
        return pending();
    };
    let Ok((url, route_target)) = api_url(
        provider_spec,
        route,
        Some(&occurrence.occurrence_id),
        settings,
    ) else {
        return pending();
    };
    if validate_api_url(
        &url,
        &route_target.path_segments,
        route_target.owner_id_query.as_deref(),
    )
    .is_err()
    {
        return pending();
    }
    let Ok(credential) = read_credential() else {
        return pending();
    };
    // The POST acknowledgement alone is never terminal evidence. Whether it
    // succeeds, fails, or times out, only an exact subsequent GET may settle.
    let _termination_ack = send_api_request(
        network,
        &url,
        "POST",
        None,
        &credential,
        deadline,
        settings,
        cancellation,
    );
    observe_termination_with_network(
        common,
        occurrence,
        termination,
        settings,
        provider_spec,
        provider_spec.termination_observation_route(),
        network,
        &credential,
        deadline,
        cancellation,
    )
}

fn observe_termination(
    common: &ryeos_external_execution_contract::LifecycleOperationCommon,
    occurrence: &ryeos_external_execution_contract::BoundOccurrence,
    termination: &ryeos_external_execution_contract::TerminationIntent,
    settings: &Settings,
    provider_spec: &ProviderSpec,
    network: &NetworkContext,
    deadline: MonotonicDeadline,
    cancellation: &NetworkCancellation,
) -> LifecycleAdapterResponse {
    let pending = || LifecycleAdapterResponse::TerminationPending {
        operation_id: common.operation_id.clone(),
        termination_request_digest: termination.termination_request_digest.clone(),
    };
    let Ok(credential) = read_credential() else {
        return pending();
    };
    if !provider_spec.reconciliation_terminal_proof_enabled() {
        return pending();
    }
    observe_termination_with_network(
        common,
        occurrence,
        termination,
        settings,
        provider_spec,
        provider_spec.reconciliation_route(),
        network,
        &credential,
        deadline,
        cancellation,
    )
}

fn observe_termination_with_network(
    common: &ryeos_external_execution_contract::LifecycleOperationCommon,
    occurrence: &ryeos_external_execution_contract::BoundOccurrence,
    termination: &ryeos_external_execution_contract::TerminationIntent,
    settings: &Settings,
    provider_spec: &ProviderSpec,
    route: Option<RouteName>,
    network: &NetworkContext,
    credential: &Zeroizing<String>,
    deadline: MonotonicDeadline,
    cancellation: &NetworkCancellation,
) -> LifecycleAdapterResponse {
    let pending = || LifecycleAdapterResponse::TerminationPending {
        operation_id: common.operation_id.clone(),
        termination_request_digest: termination.termination_request_digest.clone(),
    };
    let Some(route) = route else {
        return pending();
    };
    let Ok((url, route_target)) = api_url(
        provider_spec,
        route,
        Some(&occurrence.occurrence_id),
        settings,
    ) else {
        return pending();
    };
    if validate_api_url(
        &url,
        &route_target.path_segments,
        route_target.owner_id_query.as_deref(),
    )
    .is_err()
    {
        return pending();
    }
    let Ok(response) = send_api_request(
        network,
        &url,
        "GET",
        None,
        credential,
        deadline,
        settings,
        cancellation,
    ) else {
        return pending();
    };
    let (status, bytes) = match read_response(response) {
        Ok(value) => value,
        Err(()) => return pending(),
    };
    if status != 200 {
        return pending();
    }
    let sandbox: RenderSandbox = match from_json_slice_strict(
        &bytes,
        usize::try_from(MAX_API_RESPONSE_BYTES).unwrap_or(usize::MAX),
    ) {
        Ok(value) => value,
        Err(_) => return pending(),
    };
    let Some(terminated_at) = exact_terminal_timestamp(&sandbox, &occurrence.occurrence_id) else {
        return pending();
    };
    let response_sha256 = lillux::sha256_hex(&bytes);
    let evidence = TerminalEvidence {
        schema: 1,
        provider: "render-sandbox-early-access",
        terminal_state: "terminated",
        operation_id: &common.operation_id,
        binding_hash: &common.binding_hash,
        request_digest: &occurrence.request_digest,
        occurrence_id: &occurrence.occurrence_id,
        termination_request_digest: &termination.termination_request_digest,
        terminated_at,
        sandbox_response_sha256: &response_sha256,
    };
    let Ok(evidence_bytes) = canonical_json(&evidence) else {
        return pending();
    };
    LifecycleAdapterResponse::OccurrenceTerminal {
        operation_id: common.operation_id.clone(),
        termination_request_digest: termination.termination_request_digest.clone(),
        provider_observation_digest: lillux::sha256_hex(&evidence_bytes),
    }
}

fn network_context_from_captured_inputs() -> Result<NetworkContext> {
    let resolver = read_sealed_env(LIFECYCLE_RESOLVER_FD_ENV, MAX_NETWORK_INPUT_BYTES)?;
    let hosts = read_sealed_env(LIFECYCLE_HOSTS_FD_ENV, MAX_NETWORK_INPUT_BYTES)?;
    let resolver_digest =
        std::env::var(LIFECYCLE_RESOLVER_SHA256_ENV).context("missing captured resolver digest")?;
    let hosts_digest =
        std::env::var(LIFECYCLE_HOSTS_SHA256_ENV).context("missing captured hosts digest")?;
    let network_policy_digest = std::env::var(LIFECYCLE_NETWORK_POLICY_SHA256_ENV)
        .context("missing captured network policy digest")?;
    ensure!(
        lillux::valid_hash(&resolver_digest)
            && lillux::valid_hash(&hosts_digest)
            && lillux::valid_hash(&network_policy_digest)
            && lillux::sha256_hex(&resolver) == resolver_digest
            && lillux::sha256_hex(&hosts) == hosts_digest,
        "captured network input identity changed"
    );
    NetworkContext::from_config_bytes(&resolver, &hosts).context("invalid captured network inputs")
}

fn validate_settings(settings: &Settings) -> Result<()> {
    ensure!(settings.schema == 1, "unsupported settings schema");
    ensure!(
        !settings.owner_id.is_empty()
            && settings.owner_id.len() <= 256
            && settings
                .owner_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
        "invalid Render owner id"
    );
    ensure!(
        !settings.region.is_empty()
            && settings.region.len() <= 128
            && settings
                .region
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)),
        "invalid Render region"
    );
    ensure!(
        valid_snapshot_id(&settings.snapshot_id),
        "invalid immutable Render snapshot id"
    );
    ensure!(
        lillux::valid_hash(&settings.base_snapshot_hash),
        "invalid admitted base snapshot digest"
    );
    ensure!(
        !settings.tls_roots_der_base64.is_empty()
            && settings.tls_roots_der_base64.len() <= MAX_ROOTS,
        "explicit Render API TLS roots are missing or excessive"
    );
    let mut total = 0usize;
    let mut previous: Option<&str> = None;
    for root in &settings.tls_roots_der_base64 {
        ensure!(
            previous.is_none_or(|value| value < root.as_str()),
            "TLS roots are not uniquely ordered"
        );
        previous = Some(root);
        ensure!(
            root.len() <= MAX_ROOT_BYTES.div_ceil(3) * 4,
            "TLS root exceeds its encoded bound"
        );
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(root)
            .context("invalid TLS root encoding")?;
        ensure!(
            !bytes.is_empty()
                && bytes.len() <= MAX_ROOT_BYTES
                && base64::engine::general_purpose::STANDARD.encode(&bytes) == *root,
            "TLS root encoding or size is invalid"
        );
        total = total
            .checked_add(bytes.len())
            .context("TLS root byte count overflow")?;
    }
    ensure!(
        total <= MAX_ROOT_TOTAL_BYTES,
        "TLS root bundle exceeds its byte bound"
    );
    Ok(())
}

fn tls_roots(settings: &Settings) -> Result<Vec<Vec<u8>>> {
    settings
        .tls_roots_der_base64
        .iter()
        .map(|root| {
            base64::engine::general_purpose::STANDARD
                .decode(root)
                .context("invalid TLS root encoding")
        })
        .collect()
}

fn read_credential() -> Result<Zeroizing<String>> {
    let bytes = read_sealed_env(LIFECYCLE_CREDENTIAL_FD_ENV, MAX_CREDENTIAL_BYTES)?;
    let bytes = Zeroizing::new(bytes);
    let credential = std::str::from_utf8(&bytes).context("Render credential is not UTF-8")?;
    ensure!(
        !credential.is_empty()
            && credential.len() <= MAX_CREDENTIAL_BYTES
            && !credential.bytes().any(|byte| byte.is_ascii_control()),
        "Render credential format is invalid"
    );
    Ok(Zeroizing::new(credential.to_owned()))
}

fn send_api_request(
    network: &NetworkContext,
    url: &url::Url,
    method: &str,
    body: Option<Vec<u8>>,
    credential: &Zeroizing<String>,
    absolute_deadline: MonotonicDeadline,
    settings: &Settings,
    cancellation: &NetworkCancellation,
) -> Result<HttpResponse> {
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("api.render.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.path().starts_with("/v1/sandboxes"),
        "Render API origin or route changed"
    );
    let mut authorization = Zeroizing::new(b"Bearer ".to_vec());
    authorization.extend_from_slice(credential.as_bytes());
    let mut headers = vec![
        Header::new("Accept", "application/json"),
        Header::new_sensitive("Authorization", authorization),
    ];
    let body = if let Some(body) = body {
        headers.push(Header::new("Content-Type", "application/json"));
        RequestBodySource::from_bytes(body)
    } else {
        RequestBodySource::from_bytes(Vec::new())
    };
    let mut limits = Limits::control_plane();
    limits.response_body_bytes = MAX_API_RESPONSE_BYTES;
    limits.response_body_wire_bytes = MAX_API_RESPONSE_BYTES * 2;
    let request = HttpRequest {
        method: method.into(),
        url: url.clone(),
        headers,
        body,
        tls_roots_der: tls_roots(settings)?,
        limits,
        deadlines: Deadlines::new(SETUP_TIMEOUT, IDLE_TIMEOUT, absolute_deadline),
        cancellation: cancellation.clone(),
    };
    HttpClient::new(network.clone())
        .execute(request)
        .context("Render API request failed")
}

fn read_response(response: HttpResponse) -> std::result::Result<(u16, Vec<u8>), ()> {
    if !has_json_content_type(&response.headers) {
        return Err(());
    }
    let mut body = Vec::new();
    response
        .body
        .take(MAX_API_RESPONSE_BYTES.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|_| ())?;
    if body.len() as u64 > MAX_API_RESPONSE_BYTES {
        return Err(());
    }
    Ok((response.status, body))
}

fn api_url(
    provider_spec: &ProviderSpec,
    route: RouteName,
    occurrence_id: Option<&str>,
    settings: &Settings,
) -> Result<(url::Url, RouteTarget)> {
    let route_target = provider_spec.route_target(route, occurrence_id, &settings.owner_id)?;
    let mut url = url::Url::parse(provider_spec.api_base())?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Render API URL cannot accept path segments"))?;
        segments.pop_if_empty();
        for segment in &route_target.path_segments {
            segments.push(segment);
        }
    }
    if let Some(owner_id) = &route_target.owner_id_query {
        url.query_pairs_mut().append_pair("ownerId", owner_id);
    }
    Ok((url, route_target))
}

fn valid_sandbox_id(id: &str) -> bool {
    id.starts_with("sbx-")
        && id.len() > "sbx-".len()
        && id.len() <= 512
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn valid_snapshot_id(id: &str) -> bool {
    id.starts_with("snp-")
        && id.len() > "snp-".len()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn validate_api_url(
    url: &url::Url,
    expected_path_segments: &[String],
    expected_owner_id: Option<&str>,
) -> Result<()> {
    let path_segments = url
        .path_segments()
        .ok_or_else(|| anyhow::anyhow!("Render API URL has no path segments"))?
        .collect::<Vec<_>>();
    let query = url.query_pairs().collect::<Vec<_>>();
    let query_matches = match expected_owner_id {
        None => query.is_empty(),
        Some(owner_id) => {
            query.len() == 1
                && query.first().is_some_and(|(key, value)| {
                    key.as_ref() == "ownerId" && value.as_ref() == owner_id
                })
        }
    };
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("api.render.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && path_segments.len() == expected_path_segments.len()
            && path_segments
                .iter()
                .zip(expected_path_segments)
                .all(|(actual, expected)| *actual == expected.as_str())
            && query_matches,
        "Render API URL is outside the fixed API origin or route"
    );
    Ok(())
}

fn request_deadline(deadline_ms: i64, enclosing: MonotonicDeadline) -> Result<MonotonicDeadline> {
    let now_ms = lillux::time::timestamp_millis();
    let remaining = deadline_ms.saturating_sub(now_ms);
    ensure!(remaining > 0, "lifecycle contact deadline elapsed");
    Ok(
        enclosing.min(MonotonicDeadline::after(Duration::from_millis(
            u64::try_from(remaining)?,
        ))),
    )
}

fn operation_deadline() -> Result<MonotonicDeadline> {
    let raw = std::env::var(LIFECYCLE_REMAINING_TIMEOUT_MS_ENV)
        .context("missing remaining lifecycle deadline")?;
    let milliseconds: u64 = raw
        .parse()
        .context("invalid remaining lifecycle deadline")?;
    ensure!(
        (1..=MAX_OPERATION_TIMEOUT_MS).contains(&milliseconds),
        "remaining lifecycle deadline is zero or exceeds its bound"
    );
    Ok(MonotonicDeadline::after(Duration::from_millis(
        milliseconds,
    )))
}

fn verify_artifact(
    authority: &lillux::InheritedDescriptorAuthority,
    expected_digest: &str,
    expected_bytes: Option<u64>,
) -> Result<LifecycleArtifactInspection> {
    authority.require_owned_executable()?;
    let observation = authority.regular_file_observation()?;
    ensure!(
        expected_bytes.is_none_or(|bytes| bytes == observation.size())
            && authority.digest_regular_file_stable_exact(&observation)? == expected_digest,
        "lifecycle artifact identity changed"
    );
    Ok(LifecycleArtifactInspection {
        descriptor: authority
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?,
        digest: expected_digest.into(),
        bytes: observation.size(),
    })
}

fn read_sealed_env(name: &str, maximum: usize) -> Result<Vec<u8>> {
    // SAFETY: the trusted lifecycle runner installs each sealed descriptor
    // exactly once before this single-threaded startup boundary.
    unsafe { lillux::read_sealed_inherited_descriptor_from_env(name, maximum) }
        .map_err(anyhow::Error::msg)
}

fn write_response<T: Serialize>(response: &T) -> Result<()> {
    let bytes = canonical_json(response)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_LIFECYCLE_RESPONSE_BYTES,
        "lifecycle response exceeds its byte bound"
    );
    std::io::stdout().write_all(&bytes)?;
    std::io::stdout().write_all(b"\n")?;
    std::io::stdout().flush()?;
    Ok(())
}

#[cfg(test)]
mod offline_fixture_tests {
    use super::*;

    fn create_projection() -> provider_spec::CreateProjection {
        provider_spec::CreateProjection {
            owner_id: "owner-fixture".into(),
            plan: PlanValue::Standard,
            region: "oregon".into(),
            timeout_seconds: 600,
            network_policy_default: provider_spec::NetworkPolicyDefault::DenyAll,
            snapshot_id: "snp-fixture-001".into(),
        }
    }

    #[test]
    fn create_fixture_binds_only_when_all_echoed_fields_match() {
        let create_body = include_bytes!("../fixtures/create-response.json");
        let response: RenderSandbox = from_json_slice_strict(
            create_body,
            MAX_API_RESPONSE_BYTES as usize,
        )
        .unwrap();
        assert!(create_response_matches(&response, &create_projection()));
        assert_eq!(
            accepted_create_response(201, create_body, &create_projection())
                .as_ref()
                .map(|value| value.id.as_str()),
            Some(response.id.as_str())
        );

        let mut altered: serde_json::Value =
            serde_json::from_slice(include_bytes!("../fixtures/create-response.json")).unwrap();
        altered["region"] = serde_json::Value::String("different-region".into());
        let altered: RenderSandbox =
            serde_json::from_value(altered).expect("fixture remains a complete response");
        assert!(!create_response_matches(&altered, &create_projection()));
    }

    #[test]
    fn snapshot_errors_and_malformed_success_remain_unbound() {
        let snapshot_error = br#"{"code":"snapshot_not_available","message":"not available"}"#;
        for status in [404, 409, 500] {
            assert!(
                accepted_create_response(status, snapshot_error, &create_projection()).is_none()
            );
        }
        assert!(accepted_create_response(201, snapshot_error, &create_projection()).is_none());
        assert!(
            accepted_create_response(
                409,
                include_bytes!("../fixtures/create-response.json"),
                &create_projection(),
            )
            .is_none()
        );
    }

    #[test]
    fn terminal_fixture_requires_the_exact_id_and_timestamp() {
        let response: RenderSandbox = from_json_slice_strict(
            include_bytes!("../fixtures/terminated-response.json"),
            MAX_API_RESPONSE_BYTES as usize,
        )
        .unwrap();
        assert_eq!(
            exact_terminal_timestamp(&response, "sbx-fixture-001"),
            Some("2026-09-24T00:01:00Z")
        );
        assert_eq!(exact_terminal_timestamp(&response, "sbx-other"), None);
    }
}
