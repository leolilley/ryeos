//! Render snapshot-creation observations for a separately owned product transfer.
//!
//! A successful create response binds an opaque provider locator to the
//! original one-shot intent. It is never evidence that the snapshot contains
//! the retained product; an independent restored-guest verifier owns that join.

use anyhow::{Context as _, Result, ensure};
use chrono::DateTime;
use ryeos_external_execution_contract::runtime_snapshot::{
    MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES, RUNTIME_SNAPSHOT_RESULT_SCHEMA,
    RuntimeSnapshotAdapterRequest, RuntimeSnapshotAdapterResponse, RuntimeSnapshotLocator,
};
use ryeos_http_transport::{
    Deadlines, Header, HttpClient, HttpRequest, HttpResponse, Limits, RequestBodySource,
};
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use zeroize::Zeroizing;

use crate::snapshot_provider_spec::{MAX_SPEC_BYTES, SnapshotProductionSpec};
use crate::{
    ADAPTER_ID, IDLE_TIMEOUT, LIFECYCLE_BOOTSTRAP_FD_ENV, LIFECYCLE_PROVIDER_SPEC_FD_ENV,
    LIFECYCLE_PROVIDER_SPEC_SHA256_ENV, LIFECYCLE_REQUEST_FD_ENV, LIFECYCLE_SETTINGS_FD_ENV,
    LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV, LIFECYCLE_SIGNED_IMPORT_FD_ENV, MAX_SETTINGS_BYTES,
    RenderPlan, SETUP_TIMEOUT, operation_deadline, read_sealed_env, request_deadline,
    tls_roots_from_base64, validate_tls_roots, verify_artifact, write_response,
};

use crate::{valid_sandbox_id, valid_snapshot_id};

const MAX_SNAPSHOT_RESPONSE_BYTES: usize = 32 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotProductionSettings {
    schema: u32,
    owner_id: String,
    region: String,
    plan: RenderPlan,
    sandbox_group_id: String,
    tls_roots_der_base64: Vec<String>,
}

impl SnapshotProductionSettings {
    fn validate_for(&self, request: &RuntimeSnapshotAdapterRequest) -> Result<()> {
        ensure!(
            self.schema == 1
                && self.sandbox_group_id == request.intent.provider_group_id
                && self.owner_id.len() <= 256
                && !self.owner_id.is_empty()
                && self
                    .owner_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                && self.region.len() <= 128
                && !self.region.is_empty()
                && self
                    .region
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "snapshot production settings differ from the retained provider scope"
        );
        validate_tls_roots(&self.tls_roots_der_base64)
    }
}

/// Only this first-claim invocation may contact Render. Reconciliation must
/// read retained journal/provider identities and never call this entry again.
pub(crate) fn operate(adapter: &lillux::InheritedDescriptorAuthority) -> Result<()> {
    let deadline = operation_deadline()?;
    ensure!(
        [
            LIFECYCLE_BOOTSTRAP_FD_ENV,
            LIFECYCLE_SIGNED_IMPORT_FD_ENV,
            LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV,
        ]
        .iter()
        .all(|name| std::env::var_os(name).is_none()),
        "snapshot producer received worker activation authority"
    );
    let request_bytes = read_sealed_env(
        LIFECYCLE_REQUEST_FD_ENV,
        MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
    )?;
    let request: RuntimeSnapshotAdapterRequest =
        ryeos_external_execution_contract::from_json_slice_strict(
            &request_bytes,
            MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
        )?;
    request.validate()?;
    ensure!(
        ryeos_external_execution_contract::canonical_json(&request)? == request_bytes
            && request.intent.provider_id == ADAPTER_ID,
        "snapshot adapter request is noncanonical or selects another provider"
    );
    verify_artifact(adapter, &request.intent.adapter_artifact_hash, None)?;
    let deadline = request_deadline(request.intent.attempt_deadline_ms, deadline)?;
    let spec_bytes = read_sealed_env(LIFECYCLE_PROVIDER_SPEC_FD_ENV, MAX_SPEC_BYTES)?;
    let captured_spec_digest = std::env::var(LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)
        .context("snapshot producer lacks captured provider spec digest")?;
    ensure!(
        captured_spec_digest == request.provider_spec_digest
            && lillux::sha256_hex(&spec_bytes) == request.provider_spec_digest,
        "snapshot provider spec changed its exact signed handoff"
    );
    let spec = SnapshotProductionSpec::parse(&spec_bytes)?;
    let settings_bytes = read_sealed_env(LIFECYCLE_SETTINGS_FD_ENV, MAX_SETTINGS_BYTES)?;
    ensure!(
        lillux::sha256_hex(&settings_bytes) == request.intent.settings_digest,
        "snapshot production settings changed"
    );
    let settings: SnapshotProductionSettings =
        ryeos_external_execution_contract::from_json_slice_strict(
            &settings_bytes,
            MAX_SETTINGS_BYTES,
        )?;
    settings.validate_for(&request)?;
    let creation_intent = creation_intent(&request, &settings);
    creation_intent.validate()?;
    // SAFETY: the trusted runner transferred this exact sealed descriptor
    // once into this single-threaded producer invocation.
    let upload = unsafe { lillux::take_inherited_descriptor_authority(request.upload_descriptor) }
        .map_err(anyhow::Error::msg)?;
    crate::activation_contact::verify_package_before_contact(
        &upload,
        request.upload_bytes,
        &request.upload_sha256,
    )?;
    let network = crate::network_context_from_captured_inputs()?;
    let cancellation = lillux::network::NetworkCancellation::default();
    let _signal_cancellation = crate::SignalCancellation::install(cancellation.clone())?;
    let credential = crate::read_credential()?;
    let result = first_snapshot_attempt(
        &request,
        &creation_intent,
        &spec,
        &settings,
        &upload,
        &network,
        &credential,
        deadline,
        &cancellation,
    )?;
    result.validate_for(&request)?;
    write_response(&result)
}

#[allow(clippy::too_many_arguments)]
fn first_snapshot_attempt(
    request: &RuntimeSnapshotAdapterRequest,
    creation_intent: &SnapshotCreationIntent,
    spec: &SnapshotProductionSpec,
    settings: &SnapshotProductionSettings,
    upload: &lillux::InheritedDescriptorAuthority,
    network: &lillux::network::NetworkContext,
    credential: &Zeroizing<String>,
    deadline: lillux::time::MonotonicDeadline,
    cancellation: &lillux::network::NetworkCancellation,
) -> Result<RuntimeSnapshotAdapterResponse> {
    let source_id = &request.intent.source_occurrence_id;
    let source_path = spec.source_status_path(source_id)?;
    let source_url = control_url(&source_path, &settings.owner_id, None)?;
    let (status, source_body) = read_control_json(send_control(
        network,
        &source_url,
        "GET",
        None,
        credential,
        settings,
        deadline,
        cancellation,
    )?)?;
    ensure!(status == 200, "snapshot source did not return exact status");
    let source: crate::RenderSandbox = ryeos_external_execution_contract::from_json_slice_strict(
        &source_body,
        MAX_SNAPSHOT_RESPONSE_BYTES,
    )?;
    ensure!(
        source.id == *source_id
            && source.status == crate::RenderSandboxStatus::Running
            && source.terminated_at.is_none()
            && source.network_policy.default == crate::RenderNetworkPolicyDefault::DenyAll
            && source.region == settings.region
            && source.timeout_seconds > 0
            && DateTime::parse_from_rfc3339(&source.created_at).is_ok()
            && matches!(
                (source.plan, settings.plan),
                (crate::RenderSandboxPlan::Starter, RenderPlan::Starter)
                    | (crate::RenderSandboxPlan::Standard, RenderPlan::Standard)
                    | (crate::RenderSandboxPlan::Pro, RenderPlan::Pro)
            ),
        "snapshot source is not the exact running denied-network sandbox"
    );

    crate::activation_contact::verify_package_before_contact(
        upload,
        request.upload_bytes,
        &request.upload_sha256,
    )?;
    let token_path = spec.source_upload_token_path(source_id)?;
    let token_url = control_url(&token_path, &settings.owner_id, Some(spec.upload_path()))?;
    let (status, token_body) = read_control_json(send_control(
        network,
        &token_url,
        "POST",
        None,
        credential,
        settings,
        deadline,
        cancellation,
    )?)?;
    ensure!(status == 201, "snapshot upload token was not accepted");
    let operation = crate::proxy_route::ProxyOperation::UploadFile {
        remote_path: spec.upload_path(),
    };
    let token = crate::proxy_route::bind_connect_response(
        &Zeroizing::new(token_body),
        source_id,
        &settings.region,
        operation,
        lillux::time::timestamp_millis(),
    )?;
    crate::proxy_route::validate_proxy_route(
        token.route.as_str(),
        &token.method,
        source_id,
        &settings.region,
        operation,
    )?;
    ensure!(
        token.expires_at_ms > lillux::time::timestamp_millis(),
        "snapshot upload proxy token expired before use"
    );
    let mut bearer = Zeroizing::new(b"Bearer ".to_vec());
    bearer.extend_from_slice(token.bearer.as_bytes());
    let mut limits = Limits::control_plane();
    limits.request_body_bytes = request.upload_bytes;
    limits.response_body_bytes = MAX_SNAPSHOT_RESPONSE_BYTES as u64;
    limits.response_body_wire_bytes = (MAX_SNAPSHOT_RESPONSE_BYTES * 2) as u64;
    let uploaded = HttpClient::new(network.clone()).execute(HttpRequest {
        method: token.method,
        url: token.route,
        headers: vec![
            Header::new_sensitive("Authorization", bearer),
            Header::new("Content-Type", spec.upload_content_type()),
            Header::new("Accept", "application/json"),
        ],
        body: RequestBodySource::from_inherited_regular_file(
            upload.clone(),
            request.upload_bytes,
            request.upload_sha256.clone(),
        ),
        tls_roots_der: tls_roots_from_base64(&settings.tls_roots_der_base64)?,
        limits,
        deadlines: Deadlines::new(SETUP_TIMEOUT, IDLE_TIMEOUT, deadline),
        cancellation: cancellation.clone(),
    })?;
    ensure!(
        (200..300).contains(&uploaded.status),
        "snapshot owner-product directory upload was not accepted"
    );

    let create_path = spec.create_path(source_id)?;
    let create_url = control_url(&create_path, &settings.owner_id, None)?;
    let body = ryeos_external_execution_contract::canonical_json(
        &serde_json::json!({"kind":"filesystem"}),
    )?;
    let (status, response_body) = read_control_json(send_control(
        network,
        &create_url,
        "POST",
        Some(body),
        credential,
        settings,
        deadline,
        cancellation,
    )?)?;
    let creation = bind_snapshot_create_response(creation_intent, status, &response_body)?;
    let locator = bind_snapshot_locator(request, &creation, settings)?;
    Ok(RuntimeSnapshotAdapterResponse::Bound { locator })
}

fn bind_snapshot_locator(
    request: &RuntimeSnapshotAdapterRequest,
    creation: &BoundSnapshotCreation,
    settings: &SnapshotProductionSettings,
) -> Result<RuntimeSnapshotLocator> {
    let provider_creation_observation = serde_json::to_value(&creation)?;
    let adapter_observation_sha256 = lillux::sha256_hex(
        &ryeos_external_execution_contract::canonical_json(&provider_creation_observation)?,
    );
    let locator = RuntimeSnapshotLocator {
        schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
        operation_id: request.intent.operation_id.clone(),
        intent_digest: request.intent.digest()?,
        source_occurrence_id: request.intent.source_occurrence_id.clone(),
        provider_group_id: settings.sandbox_group_id.clone(),
        snapshot_id: creation.snapshot_id.clone(),
        provider_response_sha256: creation.response_sha256.clone(),
        provider_creation_observation,
        adapter_observation_sha256,
    };
    locator.validate_for(&request.intent)?;
    Ok(locator)
}

fn retained_snapshot_creation(
    request: &RuntimeSnapshotAdapterRequest,
    intent: &SnapshotCreationIntent,
    locator: &RuntimeSnapshotLocator,
) -> Result<BoundSnapshotCreation> {
    locator.validate_for(&request.intent)?;
    ensure!(
        locator.schema == RUNTIME_SNAPSHOT_RESULT_SCHEMA
            && locator.operation_id == intent.operation_id
            && locator.source_occurrence_id == intent.source_sandbox_id
            && locator.provider_group_id == intent.sandbox_group_id,
        "retained snapshot locator differs from the creation intent"
    );
    let observation =
        ryeos_external_execution_contract::canonical_json(&locator.provider_creation_observation)?;
    ensure!(
        observation.len() <= 4096
            && lillux::sha256_hex(&observation) == locator.adapter_observation_sha256,
        "retained snapshot creation observation changed"
    );
    let creation: BoundSnapshotCreation =
        ryeos_external_execution_contract::from_json_slice_strict(&observation, 4096)?;
    ensure!(
        creation.schema == 1
            && creation.operation_id == intent.operation_id
            && creation.intent_digest == intent.digest()?
            && creation.product_witness_hash == intent.product_witness_hash
            && creation.guest_runtime_manifest_hash == intent.guest_runtime_manifest_hash
            && creation.controller_public_root == intent.controller_public_root
            && creation.owner_executable_sha256 == intent.owner_executable_sha256
            && creation.source_sandbox_id == intent.source_sandbox_id
            && creation.sandbox_group_id == intent.sandbox_group_id
            && creation.snapshot_id == locator.snapshot_id
            && creation.response_sha256 == locator.provider_response_sha256,
        "retained snapshot creation is not bound to its exact locator"
    );
    Ok(creation)
}

pub(crate) fn observe_snapshot_available_from_locator(
    request: &RuntimeSnapshotAdapterRequest,
    intent: &SnapshotCreationIntent,
    locator: &RuntimeSnapshotLocator,
    status: u16,
    body: &[u8],
) -> Result<AvailableSnapshotObservation> {
    let creation = retained_snapshot_creation(request, intent, locator)?;
    observe_snapshot_available(intent, &creation, status, body)
}

fn creation_intent(
    request: &RuntimeSnapshotAdapterRequest,
    settings: &SnapshotProductionSettings,
) -> SnapshotCreationIntent {
    SnapshotCreationIntent {
        schema: 1,
        operation_id: request.intent.operation_id.clone(),
        product_witness_hash: request.intent.product_witness_hash.clone(),
        guest_runtime_manifest_hash: request.intent.guest_runtime_manifest_hash.clone(),
        controller_public_root: request.intent.controller_public_root.clone(),
        owner_executable_sha256: request.intent.owner_executable_sha256.clone(),
        owner_id: settings.owner_id.clone(),
        sandbox_group_id: settings.sandbox_group_id.clone(),
        source_sandbox_id: request.intent.source_occurrence_id.clone(),
        plan: settings.plan,
    }
}

fn control_url(path: &str, owner_id: &str, upload_path: Option<&str>) -> Result<url::Url> {
    ensure!(
        path.starts_with("/v1/sandboxes/") && !path.contains(".."),
        "snapshot route is not admitted"
    );
    let mut url = url::Url::parse("https://api.render.com")?;
    url.set_path(path);
    ensure!(
        url.path() == path,
        "snapshot route changed during URL construction"
    );
    url.query_pairs_mut().append_pair("ownerId", owner_id);
    if let Some(upload_path) = upload_path {
        url.query_pairs_mut().append_pair("path", upload_path);
    }
    Ok(url)
}

fn snapshot_readiness_url(
    spec: &SnapshotProductionSpec,
    locator: &RuntimeSnapshotLocator,
    owner_id: &str,
) -> Result<url::Url> {
    let path = spec.get_path(&locator.provider_group_id, &locator.snapshot_id)?;
    ensure!(
        path.starts_with("/v1/sandbox-groups/") && !path.contains(".."),
        "snapshot readiness route is not admitted"
    );
    let mut url = url::Url::parse("https://api.render.com")?;
    url.set_path(&path);
    ensure!(url.path() == path, "snapshot readiness route changed");
    url.query_pairs_mut().append_pair("ownerId", owner_id);
    Ok(url)
}

#[allow(clippy::too_many_arguments)]
fn send_control(
    network: &lillux::network::NetworkContext,
    url: &url::Url,
    method: &str,
    body: Option<Vec<u8>>,
    credential: &Zeroizing<String>,
    settings: &SnapshotProductionSettings,
    deadline: lillux::time::MonotonicDeadline,
    cancellation: &lillux::network::NetworkCancellation,
) -> Result<HttpResponse> {
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("api.render.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && matches!(
                (method, body.is_some()),
                ("GET", false) | ("POST", false) | ("POST", true)
            ),
        "snapshot control request is outside its closed Render profile"
    );
    let mut bearer = Zeroizing::new(b"Bearer ".to_vec());
    bearer.extend_from_slice(credential.as_bytes());
    let mut headers = vec![
        Header::new_sensitive("Authorization", bearer),
        Header::new("Accept", "application/json"),
    ];
    let body = if let Some(body) = body {
        headers.push(Header::new("Content-Type", "application/json"));
        RequestBodySource::from_bytes(body)
    } else {
        RequestBodySource::from_bytes(Vec::new())
    };
    let mut limits = Limits::control_plane();
    limits.response_body_bytes = MAX_SNAPSHOT_RESPONSE_BYTES as u64;
    limits.response_body_wire_bytes = (MAX_SNAPSHOT_RESPONSE_BYTES * 2) as u64;
    HttpClient::new(network.clone())
        .execute(HttpRequest {
            method: method.into(),
            url: url.clone(),
            headers,
            body,
            tls_roots_der: tls_roots_from_base64(&settings.tls_roots_der_base64)?,
            limits,
            deadlines: Deadlines::new(SETUP_TIMEOUT, IDLE_TIMEOUT, deadline),
            cancellation: cancellation.clone(),
        })
        .context("snapshot control request failed")
}

fn read_control_json(response: HttpResponse) -> Result<(u16, Vec<u8>)> {
    ensure!(
        crate::has_json_content_type(&response.headers),
        "snapshot response was not JSON"
    );
    let mut body = Vec::new();
    response
        .body
        .take(MAX_SNAPSHOT_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut body)?;
    ensure!(
        !body.is_empty() && body.len() <= MAX_SNAPSHOT_RESPONSE_BYTES,
        "snapshot response exceeds its bound"
    );
    Ok((response.status, body))
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotCreationIntent {
    pub schema: u32,
    pub operation_id: String,
    pub product_witness_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub controller_public_root: String,
    pub owner_executable_sha256: String,
    pub owner_id: String,
    pub sandbox_group_id: String,
    pub source_sandbox_id: String,
    pub plan: RenderPlan,
}

impl SnapshotCreationIntent {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported snapshot creation intent");
        for hash in [
            &self.product_witness_hash,
            &self.guest_runtime_manifest_hash,
            &self.owner_executable_sha256,
        ] {
            ensure!(
                lillux::valid_hash(hash),
                "snapshot source identity is invalid"
            );
        }
        ensure!(
            self.operation_id.len() <= 256
                && !self.operation_id.is_empty()
                && self
                    .operation_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "snapshot creation operation is invalid"
        );
        ensure!(
            self.owner_id.len() <= 256
                && !self.owner_id.is_empty()
                && self
                    .owner_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "snapshot creation owner is invalid"
        );
        ensure!(
            valid_sandbox_id(&self.source_sandbox_id)
                && self.sandbox_group_id.starts_with("sbg-")
                && self.sandbox_group_id.len() <= 256
                && self
                    .sandbox_group_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "snapshot creation source is invalid"
        );
        let root = self
            .controller_public_root
            .strip_prefix("ed25519:")
            .ok_or_else(|| anyhow::anyhow!("snapshot source has no controller public root"))?;
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, root)?;
        let bytes: [u8; 32] = decoded
            .try_into()
            .map_err(|_| anyhow::anyhow!("snapshot controller root has wrong length"))?;
        let key = lillux::crypto::VerifyingKey::from_bytes(&bytes)?;
        ensure!(
            !key.is_weak()
                && base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    key.to_bytes()
                ) == root,
            "snapshot controller root is weak or noncanonical"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(self)?)?.as_bytes(),
        ))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RenderSnapshotCreateResponse {
    #[serde(deserialize_with = "required_nullable")]
    captured_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    error: Option<String>,
    expires_at: String,
    id: String,
    kind: SnapshotKind,
    #[serde(deserialize_with = "required_nullable")]
    name: Option<String>,
    plan: RenderPlan,
    requested_at: String,
    sandbox_group_id: String,
    #[serde(deserialize_with = "required_nullable")]
    size_bytes: Option<i64>,
    source_sandbox_id: String,
    status: SnapshotStatus,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SnapshotKind {
    Filesystem,
    Runtime,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SnapshotStatus {
    Creating,
    Available,
    Failed,
}

fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// A provider locator, not an installed-runtime or product qualification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BoundSnapshotCreation {
    pub schema: u32,
    pub operation_id: String,
    pub intent_digest: String,
    pub product_witness_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub controller_public_root: String,
    pub owner_executable_sha256: String,
    pub source_sandbox_id: String,
    pub sandbox_group_id: String,
    pub snapshot_id: String,
    pub requested_at: String,
    pub expires_at: String,
    pub response_sha256: String,
}

/// Provider readiness for the same opaque locator. It is not evidence of the
/// filesystem restored from this snapshot.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AvailableSnapshotObservation {
    pub schema: u32,
    pub operation_id: String,
    pub intent_digest: String,
    pub snapshot_id: String,
    pub source_sandbox_id: String,
    pub sandbox_group_id: String,
    pub captured_at: String,
    pub size_bytes: i64,
    pub creation_response_sha256: String,
    pub availability_response_sha256: String,
}

pub(crate) fn bind_snapshot_create_response(
    intent: &SnapshotCreationIntent,
    status: u16,
    body: &[u8],
) -> Result<BoundSnapshotCreation> {
    intent.validate()?;
    ensure!(
        status == 202 && !body.is_empty() && body.len() <= MAX_SNAPSHOT_RESPONSE_BYTES,
        "snapshot creation has no complete accepted provider response"
    );
    // Deserialize directly: a generic Value can silently collapse duplicate
    // keys before the exact response is checked.
    let mut decoder = serde_json::Deserializer::from_slice(body);
    let snapshot = RenderSnapshotCreateResponse::deserialize(&mut decoder)?;
    decoder.end()?;
    ensure!(
        valid_snapshot_id(&snapshot.id)
            && snapshot.kind == SnapshotKind::Filesystem
            && snapshot.source_sandbox_id == intent.source_sandbox_id
            && snapshot.sandbox_group_id == intent.sandbox_group_id
            && snapshot.plan == intent.plan
            && snapshot.name.is_none()
            && snapshot.error.is_none(),
        "snapshot create response differs from exact source or filesystem kind"
    );
    let requested = DateTime::parse_from_rfc3339(&snapshot.requested_at)?;
    let expires = DateTime::parse_from_rfc3339(&snapshot.expires_at)?;
    ensure!(
        expires > requested,
        "snapshot expiry is not after its request"
    );
    match snapshot.status {
        SnapshotStatus::Creating => ensure!(
            snapshot.captured_at.is_none() && snapshot.size_bytes.is_none(),
            "creating snapshot claims captured content"
        ),
        SnapshotStatus::Available => ensure!(
            snapshot
                .captured_at
                .as_deref()
                .is_some_and(|value| DateTime::parse_from_rfc3339(value)
                    .is_ok_and(|captured| captured >= requested && captured < expires))
                && snapshot.size_bytes.is_some_and(|bytes| bytes > 0),
            "available snapshot lacks complete capture metadata"
        ),
        SnapshotStatus::Failed => anyhow::bail!("provider reported failed snapshot creation"),
    }
    Ok(BoundSnapshotCreation {
        schema: 1,
        operation_id: intent.operation_id.clone(),
        intent_digest: intent.digest()?,
        product_witness_hash: intent.product_witness_hash.clone(),
        guest_runtime_manifest_hash: intent.guest_runtime_manifest_hash.clone(),
        controller_public_root: intent.controller_public_root.clone(),
        owner_executable_sha256: intent.owner_executable_sha256.clone(),
        source_sandbox_id: intent.source_sandbox_id.clone(),
        sandbox_group_id: intent.sandbox_group_id.clone(),
        snapshot_id: snapshot.id,
        requested_at: snapshot.requested_at,
        expires_at: snapshot.expires_at,
        response_sha256: lillux::sha256_hex(body),
    })
}

pub(crate) fn observe_snapshot_available(
    intent: &SnapshotCreationIntent,
    creation: &BoundSnapshotCreation,
    status: u16,
    body: &[u8],
) -> Result<AvailableSnapshotObservation> {
    intent.validate()?;
    ensure!(
        creation.schema == 1
            && creation.operation_id == intent.operation_id
            && creation.intent_digest == intent.digest()?
            && creation.product_witness_hash == intent.product_witness_hash
            && creation.guest_runtime_manifest_hash == intent.guest_runtime_manifest_hash
            && creation.controller_public_root == intent.controller_public_root
            && creation.owner_executable_sha256 == intent.owner_executable_sha256
            && creation.source_sandbox_id == intent.source_sandbox_id
            && creation.sandbox_group_id == intent.sandbox_group_id
            && valid_snapshot_id(&creation.snapshot_id)
            && lillux::valid_hash(&creation.response_sha256),
        "snapshot readiness is not bound to its exact creation"
    );
    ensure!(
        status == 200 && !body.is_empty() && body.len() <= MAX_SNAPSHOT_RESPONSE_BYTES,
        "snapshot readiness has no complete provider response"
    );
    let mut decoder = serde_json::Deserializer::from_slice(body);
    let snapshot = RenderSnapshotCreateResponse::deserialize(&mut decoder)?;
    decoder.end()?;
    ensure!(
        snapshot.id == creation.snapshot_id
            && snapshot.kind == SnapshotKind::Filesystem
            && snapshot.status == SnapshotStatus::Available
            && snapshot.source_sandbox_id == intent.source_sandbox_id
            && snapshot.sandbox_group_id == intent.sandbox_group_id
            && snapshot.plan == intent.plan
            && snapshot.name.is_none()
            && snapshot.error.is_none()
            && snapshot.requested_at == creation.requested_at
            && snapshot.expires_at == creation.expires_at,
        "available snapshot differs from its exact creation"
    );
    let requested = DateTime::parse_from_rfc3339(&snapshot.requested_at)?;
    let expires = DateTime::parse_from_rfc3339(&snapshot.expires_at)?;
    let captured_at = snapshot
        .captured_at
        .context("available snapshot has no capture timestamp")?;
    let captured = DateTime::parse_from_rfc3339(&captured_at)?;
    let size_bytes = snapshot
        .size_bytes
        .context("available snapshot has no byte count")?;
    ensure!(
        requested <= captured && captured < expires && size_bytes > 0,
        "available snapshot has invalid capture metadata"
    );
    Ok(AvailableSnapshotObservation {
        schema: 1,
        operation_id: intent.operation_id.clone(),
        intent_digest: creation.intent_digest.clone(),
        snapshot_id: creation.snapshot_id.clone(),
        source_sandbox_id: creation.source_sandbox_id.clone(),
        sandbox_group_id: creation.sandbox_group_id.clone(),
        captured_at,
        size_bytes,
        creation_response_sha256: creation.response_sha256.clone(),
        availability_response_sha256: lillux::sha256_hex(body),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn intent() -> SnapshotCreationIntent {
        let key = lillux::crypto::SigningKey::from_bytes(&[43; 32]).verifying_key();
        SnapshotCreationIntent {
            schema: 1,
            operation_id: "snapshot-op-1".into(),
            product_witness_hash: "1".repeat(64),
            guest_runtime_manifest_hash: "2".repeat(64),
            controller_public_root: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(key.to_bytes())
            ),
            owner_executable_sha256: "3".repeat(64),
            owner_id: "owner".into(),
            sandbox_group_id: "sbg-exact".into(),
            source_sandbox_id: "sbx-exact".into(),
            plan: RenderPlan::Starter,
        }
    }

    fn response() -> serde_json::Value {
        serde_json::json!({
            "capturedAt": null, "error": null,
            "expiresAt": "2026-10-01T00:00:00Z", "id": "snp-exact",
            "kind": "filesystem", "name": null, "plan": "starter",
            "requestedAt": "2026-09-28T00:00:00Z",
            "sandboxGroupId": "sbg-exact", "sizeBytes": null,
            "sourceSandboxId": "sbx-exact", "status": "creating"
        })
    }

    #[test]
    fn sealed_attempt_preflight_binds_source_root_and_closed_routes() {
        let source = intent();
        let mut durable =
            ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotIntent {
                schema: 1,
                operation_id: String::new(),
                owner_principal: format!("fp:{}", "1".repeat(64)),
                provider_id: ADAPTER_ID.into(),
                source_occurrence_id: source.source_sandbox_id.clone(),
                provider_group_id: source.sandbox_group_id.clone(),
                production_profile_digest: "2".repeat(64),
                adapter_artifact_hash: "3".repeat(64),
                provider_spec_digest: "4".repeat(64),
                settings_digest: "5".repeat(64),
                product_witness_hash: source.product_witness_hash.clone(),
                guest_runtime_manifest_hash: source.guest_runtime_manifest_hash.clone(),
                owner_executable_sha256: source.owner_executable_sha256.clone(),
                controller_public_root: source.controller_public_root.clone(),
                upload_sha256: "6".repeat(64),
                upload_bytes: 1024,
                attempt_deadline_ms: 1_800_000_000_000,
            };
        durable.operation_id = durable.derived_operation_id().unwrap();
        let request = RuntimeSnapshotAdapterRequest {
            protocol: ryeos_external_execution_contract::runtime_snapshot::RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL.into(),
            provider_spec_digest: durable.provider_spec_digest.clone(),
            upload_descriptor: 11,
            upload_bytes: durable.upload_bytes,
            upload_sha256: durable.upload_sha256.clone(),
            intent: durable,
        };
        request.validate().unwrap();
        let settings = SnapshotProductionSettings {
            schema: 1,
            owner_id: source.owner_id.clone(),
            region: "oregon".into(),
            plan: source.plan,
            sandbox_group_id: source.sandbox_group_id.clone(),
            tls_roots_der_base64: vec![base64::engine::general_purpose::STANDARD.encode(b"root")],
        };
        settings.validate_for(&request).unwrap();
        let prepared = creation_intent(&request, &settings);
        prepared.validate().unwrap();
        assert_eq!(
            prepared.controller_public_root,
            source.controller_public_root
        );
        let profile = SnapshotProductionSpec::parse(include_bytes!(
            "../fixtures/snapshot-production-spec.json"
        ))
        .unwrap();
        let url = control_url(
            &profile
                .source_upload_token_path(&source.source_sandbox_id)
                .unwrap(),
            &settings.owner_id,
            Some(profile.upload_path()),
        )
        .unwrap();
        assert_eq!(url.host_str(), Some("api.render.com"));
        assert_eq!(url.query_pairs().count(), 2);
        let created = bind_snapshot_create_response(
            &prepared,
            202,
            &serde_json::to_vec(&response()).unwrap(),
        )
        .unwrap();
        let locator = bind_snapshot_locator(&request, &created, &settings).unwrap();
        let readiness_url = snapshot_readiness_url(&profile, &locator, &settings.owner_id).unwrap();
        assert_eq!(
            readiness_url.path(),
            "/v1/sandbox-groups/sbg-exact/snapshots/snp-exact"
        );
        assert_eq!(readiness_url.query_pairs().count(), 1);
        let retained: BoundSnapshotCreation =
            serde_json::from_value(locator.provider_creation_observation.clone()).unwrap();
        assert_eq!(retained.requested_at, created.requested_at);
        assert_eq!(retained.expires_at, created.expires_at);
        assert_eq!(retained.response_sha256, locator.provider_response_sha256);
        assert_eq!(
            retained_snapshot_creation(&request, &prepared, &locator)
                .unwrap()
                .snapshot_id,
            created.snapshot_id
        );
        let mut substituted = locator.clone();
        substituted.snapshot_id = "snp-other".into();
        assert!(retained_snapshot_creation(&request, &prepared, &substituted).is_err());
        let mut available = response();
        available["status"] = serde_json::json!("available");
        available["capturedAt"] = serde_json::json!("2026-09-28T00:01:00Z");
        available["sizeBytes"] = serde_json::json!(4096);
        let availability_body = serde_json::to_vec(&available).unwrap();
        let observed = observe_snapshot_available_from_locator(
            &request,
            &prepared,
            &locator,
            200,
            &availability_body,
        )
        .unwrap();
        assert_eq!(observed.snapshot_id, locator.snapshot_id);
        let mut mismatched_creation = locator.clone();
        mismatched_creation.provider_creation_observation["requested_at"] =
            serde_json::json!("2026-09-27T00:00:00Z");
        mismatched_creation.adapter_observation_sha256 = lillux::sha256_hex(
            &ryeos_external_execution_contract::canonical_json(
                &mismatched_creation.provider_creation_observation,
            )
            .unwrap(),
        );
        assert!(
            observe_snapshot_available_from_locator(
                &request,
                &prepared,
                &mismatched_creation,
                200,
                &availability_body,
            )
            .is_err()
        );
        let mut wrong = settings;
        wrong.sandbox_group_id = "sbg-other".into();
        assert!(wrong.validate_for(&request).is_err());
        assert!(control_url("/v1/sandboxes/../other", "owner", None).is_err());
    }

    #[test]
    fn accepted_create_only_binds_a_locator_for_the_exact_source() {
        let intent = intent();
        let body = serde_json::to_vec(&response()).unwrap();
        let bound = bind_snapshot_create_response(&intent, 202, &body).unwrap();
        assert_eq!(bound.snapshot_id, "snp-exact");
        assert_eq!(bound.product_witness_hash, intent.product_witness_hash);
        assert_eq!(bound.intent_digest, intent.digest().unwrap());
        assert!(bind_snapshot_create_response(&intent, 201, &body).is_err());
        for (field, value) in [
            ("kind", "runtime"),
            ("sourceSandboxId", "sbx-other"),
            ("sandboxGroupId", "sbg-other"),
            ("status", "failed"),
        ] {
            let mut changed = response();
            changed[field] = serde_json::json!(value);
            assert!(
                bind_snapshot_create_response(&intent, 202, &serde_json::to_vec(&changed).unwrap())
                    .is_err()
            );
        }
        let duplicated = String::from_utf8(body).unwrap().replace(
            "\"id\":\"snp-exact\"",
            "\"id\":\"snp-exact\",\"id\":\"snp-other\"",
        );
        assert!(bind_snapshot_create_response(&intent, 202, duplicated.as_bytes()).is_err());
        let mut missing = response();
        missing.as_object_mut().unwrap().remove("sizeBytes");
        assert!(
            bind_snapshot_create_response(&intent, 202, &serde_json::to_vec(&missing).unwrap())
                .is_err()
        );
    }

    #[test]
    fn availability_is_an_exact_readiness_observation_not_content_proof() {
        let intent = intent();
        let creation =
            bind_snapshot_create_response(&intent, 202, &serde_json::to_vec(&response()).unwrap())
                .unwrap();
        let mut available = response();
        available["status"] = serde_json::json!("available");
        available["capturedAt"] = serde_json::json!("2026-09-28T00:01:00Z");
        available["sizeBytes"] = serde_json::json!(4096);
        let body = serde_json::to_vec(&available).unwrap();
        let observed = observe_snapshot_available(&intent, &creation, 200, &body).unwrap();
        assert_eq!(observed.snapshot_id, creation.snapshot_id);
        assert_eq!(observed.creation_response_sha256, creation.response_sha256);
        assert_eq!(
            observed.availability_response_sha256,
            lillux::sha256_hex(&body)
        );
        assert!(observe_snapshot_available(&intent, &creation, 202, &body).is_err());
        for (field, replacement) in [
            ("status", serde_json::json!("creating")),
            ("kind", serde_json::json!("runtime")),
            ("sourceSandboxId", serde_json::json!("sbx-other")),
            ("id", serde_json::json!("snp-other")),
            ("requestedAt", serde_json::json!("2026-09-27T00:00:00Z")),
            ("sizeBytes", serde_json::json!(0)),
        ] {
            let mut changed = available.clone();
            changed[field] = replacement;
            assert!(
                observe_snapshot_available(
                    &intent,
                    &creation,
                    200,
                    &serde_json::to_vec(&changed).unwrap()
                )
                .is_err(),
                "{field}"
            );
        }
        let duplicated = String::from_utf8(body).unwrap().replace(
            "\"id\":\"snp-exact\"",
            "\"id\":\"snp-exact\",\"id\":\"snp-other\"",
        );
        assert!(
            observe_snapshot_available(&intent, &creation, 200, duplicated.as_bytes()).is_err()
        );
    }
}
