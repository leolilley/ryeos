//! Render's unsnapshotted source Sandbox for a checked materialization.
//! This is a one-attempt create handler, not Worker allocation authority.

use anyhow::{Context as _, Result, ensure};
use chrono::DateTime;
use ryeos_external_execution_contract::runtime_snapshot_bootstrap::{
    BootstrapCleanupMode, MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES,
    RuntimeSnapshotBootstrapAdapterRequest, RuntimeSnapshotBootstrapAdapterResponse,
    RuntimeSnapshotBootstrapOccurrence, RuntimeSnapshotBootstrapReadinessAdapterResponse,
    RuntimeSnapshotBootstrapReadinessObservation, RuntimeSnapshotBootstrapReadinessRequest,
    RuntimeSnapshotBootstrapTerminalObservation, RuntimeSnapshotBootstrapTerminationAdapterRequest,
    RuntimeSnapshotBootstrapTerminationAdapterResponse,
};
use ryeos_external_execution_contract::{
    MAX_LIFECYCLE_PROVIDER_SPEC_BYTES, canonical_json, from_json_slice_strict,
};
use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::provider_spec::{NetworkPolicyDefault, PlanValue, ProviderSpec, RouteName};
use crate::{RenderApiSettings, RenderPlan, RenderSandboxPlan, RenderSandboxStatus};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BootstrapSettings {
    schema: u32,
    owner_id: String,
    plan: RenderPlan,
    region: String,
    sandbox_group_id: String,
    tls_roots_der_base64: Vec<String>,
}

impl RenderApiSettings for BootstrapSettings {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }
    fn tls_roots_der_base64(&self) -> &[String] {
        &self.tls_roots_der_base64
    }
}

fn selected_settings(
    bytes: &[u8],
    request: &RuntimeSnapshotBootstrapAdapterRequest,
) -> Result<BootstrapSettings> {
    ensure!(
        lillux::sha256_hex(bytes) == request.intent.settings_digest,
        "bootstrap settings differ from sealed protected selection"
    );
    let settings: BootstrapSettings = from_json_slice_strict(bytes, crate::MAX_SETTINGS_BYTES)?;
    ensure!(
        settings.schema == 1
            && canonical_json(&settings)? == bytes
            && settings.sandbox_group_id == request.intent.provider_group_id
            && settings.sandbox_group_id.starts_with("sbg-")
            && settings.sandbox_group_id.len() <= 256
            && settings
                .sandbox_group_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "bootstrap settings differ from protected group"
    );
    crate::validate_common_settings(
        &settings.owner_id,
        &settings.region,
        &settings.tls_roots_der_base64,
    )?;
    Ok(settings)
}

pub(crate) fn create_source_sandbox(adapter: &lillux::InheritedDescriptorAuthority) -> Result<()> {
    let enclosing = crate::operation_deadline()?;
    ensure!(
        [
            crate::LIFECYCLE_BOOTSTRAP_FD_ENV,
            crate::LIFECYCLE_SIGNED_IMPORT_FD_ENV,
            crate::LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV,
        ]
        .iter()
        .all(|name| std::env::var_os(name).is_none()),
        "bootstrap source received Worker activation authority"
    );
    let bytes = crate::read_sealed_env(
        crate::LIFECYCLE_REQUEST_FD_ENV,
        MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES,
    )?;
    let request: RuntimeSnapshotBootstrapAdapterRequest =
        from_json_slice_strict(&bytes, MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES)?;
    request.validate()?;
    let intent_digest = request.intent.digest()?;
    ensure!(
        canonical_json(&request)? == bytes && request.intent.provider_id == crate::ADAPTER_ID,
        "bootstrap request is noncanonical or selects another provider"
    );
    crate::verify_artifact(adapter, &request.intent.adapter_artifact_hash, None)?;
    let deadline = crate::request_deadline(request.intent.attempt_deadline_ms, enclosing)?;
    let spec_bytes = crate::read_sealed_env(
        crate::LIFECYCLE_PROVIDER_SPEC_FD_ENV,
        usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )?;
    let spec_digest = std::env::var(crate::LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)
        .context("bootstrap create lacks captured provider spec digest")?;
    ensure!(
        spec_digest == request.provider_spec_digest
            && lillux::sha256_hex(&spec_bytes) == request.provider_spec_digest,
        "bootstrap provider spec changed its signed handoff"
    );
    let spec = ProviderSpec::parse(
        &spec_bytes,
        &lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json")),
    )?;
    ensure!(
        spec.bootstrap_profile_digest() == request.intent.bootstrap_profile_digest,
        "bootstrap operation differs from signed profile"
    );
    let settings_bytes =
        crate::read_sealed_env(crate::LIFECYCLE_SETTINGS_FD_ENV, crate::MAX_SETTINGS_BYTES)?;
    let settings = selected_settings(&settings_bytes, &request)?;
    let plan = match settings.plan {
        RenderPlan::Starter => PlanValue::Starter,
        RenderPlan::Standard => PlanValue::Standard,
        RenderPlan::Pro => PlanValue::Pro,
    };
    let projection = spec.bootstrap_create_projection(
        &settings.owner_id,
        plan,
        &settings.region,
        request.intent.maximum_lifetime_seconds,
    )?;
    ensure!(
        projection.network_policy_default == NetworkPolicyDefault::DenyAll,
        "bootstrap profile changed deny-all policy"
    );
    let route = spec
        .bootstrap_create_route()
        .context("signed provider spec has no bootstrap route")?;
    let (url, target) = crate::api_url(&spec, route, None, &settings, None)?;
    crate::validate_api_url(
        &url,
        &target.path_segments,
        target.owner_id_query.as_deref(),
        None,
    )?;
    let body = crate::CreateSandboxBody {
        owner_id: projection.owner_id.clone(),
        plan: settings.plan,
        region: projection.region.clone(),
        timeout_seconds: projection.timeout_seconds,
        network_policy: crate::NetworkPolicy {
            default: crate::RenderNetworkPolicyDefault::DenyAll,
        },
        snapshot_id: None,
    };
    let body = canonical_json(&body)?;
    let network = crate::network_context_from_captured_inputs()?;
    let cancellation = lillux::network::NetworkCancellation::default();
    let _signal = crate::SignalCancellation::install(cancellation.clone())?;
    let credential = crate::read_credential()?;
    let result = match crate::send_api_request(
        &network,
        &url,
        "POST",
        Some(body),
        &credential,
        deadline,
        &settings,
        &cancellation,
    ) {
        Ok(response) => match crate::read_response(response) {
            Ok((201, body)) => {
                let cleanup_id = from_json_slice_strict::<CleanupSandboxId>(
                    &body,
                    usize::try_from(crate::MAX_API_RESPONSE_BYTES)?,
                );
                match cleanup_id {
                    Ok(cleanup_id) if crate::valid_sandbox_id(&cleanup_id.id) => {
                        let occurrence = match from_json_slice_strict::<crate::RenderSandbox>(
                            &body,
                            usize::try_from(crate::MAX_API_RESPONSE_BYTES)?,
                        ) {
                            Ok(sandbox) if sandbox.id == cleanup_id.id => occurrence_from_create(
                                sandbox,
                                &body,
                                &projection,
                                &request.intent.operation_id,
                                &settings.owner_id,
                            ),
                            _ => cleanup_only_occurrence(
                                cleanup_id.id,
                                &body,
                                &request.intent.operation_id,
                                &settings.owner_id,
                            ),
                        };
                        RuntimeSnapshotBootstrapAdapterResponse::OccurrenceBound { occurrence }
                    }
                    _ => uncertain(&request, &intent_digest),
                }
            }
            _ => uncertain(&request, &intent_digest),
        },
        Err(_) => uncertain(&request, &intent_digest),
    };
    result.validate_for(&request)?;
    crate::write_response(&result)
}

pub(crate) fn observe_source_readiness(
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
        "bootstrap observer received Worker activation authority"
    );
    let bytes = crate::read_sealed_env(
        crate::LIFECYCLE_REQUEST_FD_ENV,
        MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES,
    )?;
    let request: RuntimeSnapshotBootstrapReadinessRequest =
        from_json_slice_strict(&bytes, MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES)?;
    request.validate()?;
    ensure!(
        canonical_json(&request)? == bytes && request.intent.provider_id == crate::ADAPTER_ID,
        "bootstrap readiness request is noncanonical or selects another provider"
    );
    crate::verify_artifact(adapter, &request.intent.adapter_artifact_hash, None)?;
    let spec_bytes = crate::read_sealed_env(
        crate::LIFECYCLE_PROVIDER_SPEC_FD_ENV,
        usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )?;
    let spec_digest = std::env::var(crate::LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)
        .context("bootstrap observer lacks captured provider spec digest")?;
    ensure!(
        spec_digest == request.provider_spec_digest
            && lillux::sha256_hex(&spec_bytes) == request.provider_spec_digest,
        "bootstrap observer provider spec changed its signed handoff"
    );
    let spec = ProviderSpec::parse(
        &spec_bytes,
        &lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json")),
    )?;
    ensure!(
        spec.bootstrap_profile_digest() == request.intent.bootstrap_profile_digest,
        "bootstrap observer profile changed"
    );
    let settings_bytes =
        crate::read_sealed_env(crate::LIFECYCLE_SETTINGS_FD_ENV, crate::MAX_SETTINGS_BYTES)?;
    let settings = selected_settings(
        &settings_bytes,
        &RuntimeSnapshotBootstrapAdapterRequest {
            protocol: ryeos_external_execution_contract::runtime_snapshot_bootstrap::BOOTSTRAP_ADAPTER_PROTOCOL.into(),
            intent: request.intent.clone(),
            provider_spec_digest: request.provider_spec_digest.clone(),
        },
    )?;
    ensure!(
        request.occurrence.provider_creation_observation["requested_owner_scope"]
            == settings.owner_id,
        "bootstrap creation owner differs from current signed scope"
    );
    let (url, target) = crate::api_url(
        &spec,
        RouteName::SandboxById,
        Some(&request.occurrence.occurrence_id),
        &settings,
        None,
    )?;
    crate::validate_api_url(
        &url,
        &target.path_segments,
        target.owner_id_query.as_deref(),
        None,
    )?;
    let network = crate::network_context_from_captured_inputs()?;
    let cancellation = lillux::network::NetworkCancellation::default();
    let _signal = crate::SignalCancellation::install(cancellation.clone())?;
    let credential = crate::read_credential()?;
    let (status, body) = crate::read_response(crate::send_api_request(
        &network,
        &url,
        "GET",
        None,
        &credential,
        deadline,
        &settings,
        &cancellation,
    )?)
    .map_err(|()| anyhow::anyhow!("bootstrap readiness response is not bounded JSON"))?;
    let not_ready = || RuntimeSnapshotBootstrapReadinessAdapterResponse::NotReady {
        operation_id: request.intent.operation_id.clone(),
        occurrence_id: request.occurrence.occurrence_id.clone(),
    };
    let result = if status == 200 {
        match from_json_slice_strict::<crate::RenderSandbox>(
            &body,
            usize::try_from(crate::MAX_API_RESPONSE_BYTES)?,
        ) {
            Ok(sandbox)
                if readiness_running_matches(
                    &sandbox,
                    &request.occurrence,
                    &spec.bootstrap_create_projection(
                        &settings.owner_id,
                        match settings.plan {
                            RenderPlan::Starter => PlanValue::Starter,
                            RenderPlan::Standard => PlanValue::Standard,
                            RenderPlan::Pro => PlanValue::Pro,
                        },
                        &settings.region,
                        request.intent.maximum_lifetime_seconds,
                    )?,
                    i64::try_from(lillux::time::timestamp_millis())?,
                ) =>
            {
                RuntimeSnapshotBootstrapReadinessAdapterResponse::Running {
                    observation: RuntimeSnapshotBootstrapReadinessObservation {
                        schema: 1,
                        operation_id: request.intent.operation_id.clone(),
                        occurrence_id: sandbox.id,
                        provider_response_sha256: lillux::sha256_hex(&body),
                        observed_created_at: sandbox.created_at,
                        observed_at_ms: i64::try_from(lillux::time::timestamp_millis())?,
                    },
                }
            }
            _ => not_ready(),
        }
    } else {
        not_ready()
    };
    result.validate_for(&request)?;
    crate::write_response(&result)
}

/// One termination POST at most, followed by a read-only exact-ID terminal
/// observation. Recovery invokes this with `first_contact=false` and never
/// repeats the mutation after an ambiguous response.
pub(crate) fn terminate_source_sandbox(
    adapter: &lillux::InheritedDescriptorAuthority,
    first_contact: bool,
) -> Result<()> {
    let enclosing = crate::operation_deadline()?;
    ensure!(
        [
            crate::LIFECYCLE_BOOTSTRAP_FD_ENV,
            crate::LIFECYCLE_SIGNED_IMPORT_FD_ENV,
            crate::LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV
        ]
        .iter()
        .all(|name| std::env::var_os(name).is_none()),
        "bootstrap termination received Worker activation authority"
    );
    let bytes = crate::read_sealed_env(
        crate::LIFECYCLE_REQUEST_FD_ENV,
        MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES,
    )?;
    let request: RuntimeSnapshotBootstrapTerminationAdapterRequest =
        from_json_slice_strict(&bytes, MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES)?;
    request.validate()?;
    ensure!(
        canonical_json(&request)? == bytes && request.intent.provider_id == crate::ADAPTER_ID,
        "bootstrap termination request is noncanonical or selects another provider"
    );
    ensure!(
        !first_contact || request.intent.mode == BootstrapCleanupMode::TerminateOnce,
        "read-only bootstrap cleanup cannot send a termination mutation"
    );
    crate::verify_artifact(
        adapter,
        &request.bootstrap_intent.adapter_artifact_hash,
        None,
    )?;
    let deadline = if first_contact {
        crate::request_deadline(request.intent.attempt_deadline_ms, enclosing)?
    } else {
        enclosing
    };
    let spec_bytes = crate::read_sealed_env(
        crate::LIFECYCLE_PROVIDER_SPEC_FD_ENV,
        usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )?;
    let spec_digest = std::env::var(crate::LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)
        .context("bootstrap termination lacks captured provider spec digest")?;
    ensure!(
        spec_digest == request.provider_spec_digest
            && lillux::sha256_hex(&spec_bytes) == request.provider_spec_digest,
        "bootstrap termination provider spec changed its signed handoff"
    );
    let spec = ProviderSpec::parse(
        &spec_bytes,
        &lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json")),
    )?;
    ensure!(
        spec.bootstrap_profile_digest() == request.bootstrap_intent.bootstrap_profile_digest,
        "bootstrap termination profile changed"
    );
    let settings_bytes =
        crate::read_sealed_env(crate::LIFECYCLE_SETTINGS_FD_ENV, crate::MAX_SETTINGS_BYTES)?;
    let settings = selected_settings(&settings_bytes,
        &RuntimeSnapshotBootstrapAdapterRequest {
            protocol: ryeos_external_execution_contract::runtime_snapshot_bootstrap::BOOTSTRAP_ADAPTER_PROTOCOL.into(),
            intent: request.bootstrap_intent.clone(),
            provider_spec_digest: request.provider_spec_digest.clone(),
        })?;
    ensure!(
        request.occurrence.provider_creation_observation["requested_owner_scope"]
            == settings.owner_id,
        "bootstrap termination changed original owner scope"
    );
    let mutation = RouteName::SandboxTerminateById;
    let observation = RouteName::SandboxById;
    for route in [mutation, observation] {
        let (url, target) = crate::api_url(
            &spec,
            route,
            Some(&request.intent.occurrence_id),
            &settings,
            None,
        )?;
        crate::validate_api_url(
            &url,
            &target.path_segments,
            target.owner_id_query.as_deref(),
            None,
        )?;
    }
    let network = crate::network_context_from_captured_inputs()?;
    let cancellation = lillux::network::NetworkCancellation::default();
    let _signal = crate::SignalCancellation::install(cancellation.clone())?;
    let credential = crate::read_credential()?;
    if first_contact {
        let (url, _) = crate::api_url(
            &spec,
            mutation,
            Some(&request.intent.occurrence_id),
            &settings,
            None,
        )?;
        let _ = crate::send_api_request(
            &network,
            &url,
            "POST",
            None,
            &credential,
            deadline,
            &settings,
            &cancellation,
        );
    }
    let pending = || RuntimeSnapshotBootstrapTerminationAdapterResponse::Uncertain {
        operation_id: request.intent.operation_id.clone(),
    };
    let (url, _) = crate::api_url(
        &spec,
        observation,
        Some(&request.intent.occurrence_id),
        &settings,
        None,
    )?;
    let exact_get = crate::send_api_request(
        &network,
        &url,
        "GET",
        None,
        &credential,
        deadline,
        &settings,
        &cancellation,
    )
    .ok()
    .and_then(|response| crate::read_response(response).ok())
    .and_then(|(status, body)| {
        if status != 200 {
            return None;
        }
        let sandbox: CleanupTerminalSandbox =
            from_json_slice_strict(&body, usize::try_from(crate::MAX_API_RESPONSE_BYTES).ok()?)
                .ok()?;
        let created_at = request.occurrence.provider_creation_observation["created_at"].as_str()?;
        let terminated_at =
            terminal_timestamp_for_cleanup(&sandbox, &request.intent.occurrence_id, created_at)?;
        Some(
            RuntimeSnapshotBootstrapTerminationAdapterResponse::Terminal {
                observation: RuntimeSnapshotBootstrapTerminalObservation {
                    schema: 1,
                    operation_id: request.intent.operation_id.clone(),
                    occurrence_id: request.intent.occurrence_id.clone(),
                    provider_response_sha256: lillux::sha256_hex(&body),
                    terminated_at: terminated_at.to_owned(),
                    contact_deadline_exceeded: false,
                },
            },
        )
    });
    let result = exact_get
        .or_else(|| {
            observe_terminated_list(
                &network,
                &spec,
                &settings,
                &credential,
                deadline,
                &cancellation,
                &request,
            )
        })
        .unwrap_or_else(pending);
    result.validate_for(&request)?;
    crate::write_response(&result)
}

fn terminal_timestamp_for_cleanup<'a>(
    sandbox: &'a CleanupTerminalSandbox,
    exact_id: &str,
    exact_created_at: &str,
) -> Option<&'a str> {
    if sandbox.id != exact_id
        || sandbox.created_at.as_deref()? != exact_created_at
        || sandbox.status != "terminated"
    {
        return None;
    }
    let at = sandbox.terminated_at.as_deref()?;
    DateTime::parse_from_rfc3339(at).ok()?;
    Some(at)
}

const TERMINATED_LIST_PAGE_LIMIT: usize = 100;
const TERMINATED_LIST_MAX_PAGES: usize = 32;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminatedListEntry {
    sandbox: CleanupTerminalSandbox,
    cursor: String,
}

/// Render's exact-ID GET may disappear after termination. A list page is
/// useful only when the owner, status filter and pagination are all exact.
/// A positive exact match is sufficient on its page; absence, a truncated
/// traversal, or a changed creation time is no proof.
fn observe_terminated_list(
    network: &crate::NetworkContext,
    spec: &ProviderSpec,
    settings: &BootstrapSettings,
    credential: &zeroize::Zeroizing<String>,
    deadline: lillux::time::MonotonicDeadline,
    cancellation: &lillux::network::NetworkCancellation,
    request: &RuntimeSnapshotBootstrapTerminationAdapterRequest,
) -> Option<RuntimeSnapshotBootstrapTerminationAdapterResponse> {
    let created_at = request.occurrence.provider_creation_observation["created_at"].as_str()?;
    DateTime::parse_from_rfc3339(created_at).ok()?;
    let route = spec.bootstrap_terminal_list_route()?;
    let (base, target) = crate::api_url(spec, route, None, settings, None).ok()?;
    crate::validate_api_url(&base, &target.path_segments, None, None).ok()?;
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::BTreeSet::new();
    let mut match_evidence: Option<(String, String)> = None;
    for _ in 0..TERMINATED_LIST_MAX_PAGES {
        let mut url = base.clone();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("ownerId", &settings.owner_id);
            query.append_pair("status", "terminated");
            query.append_pair("limit", "100");
            if let Some(cursor) = &cursor {
                query.append_pair("cursor", cursor);
            }
        }
        let query = url.query_pairs().collect::<Vec<_>>();
        if query.len() != 3 + usize::from(cursor.is_some())
            || query[0].0 != "ownerId"
            || query[0].1 != settings.owner_id
            || query[1].0 != "status"
            || query[1].1 != "terminated"
            || query[2].0 != "limit"
            || query[2].1 != "100"
            || cursor
                .as_ref()
                .is_some_and(|value| query[3].0 != "cursor" || query[3].1 != *value)
        {
            return None;
        }
        let (status, body) = crate::read_response(
            crate::send_api_request(
                network,
                &url,
                "GET",
                None,
                credential,
                deadline,
                settings,
                cancellation,
            )
            .ok()?,
        )
        .ok()?;
        if status != 200 {
            return None;
        }
        let (count, next_cursor) = scan_terminated_page(
            &body,
            &request.intent.occurrence_id,
            created_at,
            &mut seen_cursors,
            &mut match_evidence,
        )?;
        if let Some((response_hash, terminated_at)) = match_evidence.take() {
            return Some(
                RuntimeSnapshotBootstrapTerminationAdapterResponse::Terminal {
                    observation: RuntimeSnapshotBootstrapTerminalObservation {
                        schema: 1,
                        operation_id: request.intent.operation_id.clone(),
                        occurrence_id: request.intent.occurrence_id.clone(),
                        provider_response_sha256: response_hash,
                        terminated_at,
                        contact_deadline_exceeded: false,
                    },
                },
            );
        }
        if count < TERMINATED_LIST_PAGE_LIMIT {
            return None;
        }
        cursor = Some(next_cursor?);
    }
    None
}

fn scan_terminated_page(
    body: &[u8],
    exact_id: &str,
    exact_created_at: &str,
    seen_cursors: &mut std::collections::BTreeSet<String>,
    match_evidence: &mut Option<(String, String)>,
) -> Option<(usize, Option<String>)> {
    let entries: Vec<TerminatedListEntry> =
        from_json_slice_strict(body, usize::try_from(crate::MAX_API_RESPONSE_BYTES).ok()?).ok()?;
    if entries.len() > TERMINATED_LIST_PAGE_LIMIT {
        return None;
    }
    for entry in &entries {
        if entry.cursor.is_empty()
            || entry.cursor.len() > 512
            || entry.cursor.bytes().any(|byte| byte.is_ascii_control())
            || !seen_cursors.insert(entry.cursor.clone())
        {
            return None;
        }
        if entry.sandbox.id == exact_id {
            if match_evidence.is_some() {
                return None;
            }
            let at = terminal_timestamp_for_cleanup(&entry.sandbox, exact_id, exact_created_at)?;
            *match_evidence = Some((lillux::sha256_hex(body), at.to_owned()));
        }
    }
    Some((
        entries.len(),
        entries.last().map(|entry| entry.cursor.clone()),
    ))
}

/// Cleanup observation deliberately reads only the fields needed to prove
/// terminal state. A provider-added plan/region field cannot erase the exact
/// cleanup target; duplicate decisive fields are refused.
struct CleanupTerminalSandbox {
    id: String,
    status: String,
    created_at: Option<String>,
    terminated_at: Option<String>,
}

impl<'de> Deserialize<'de> for CleanupTerminalSandbox {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct TerminalVisitor;
        impl<'de> Visitor<'de> for TerminalVisitor {
            type Value = CleanupTerminalSandbox;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a Render terminal sandbox object")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut id = None;
                let mut status = None;
                let mut created_at = None;
                let mut terminated_at = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            id = Some(map.next_value::<String>()?);
                        }
                        "status" => {
                            if status.is_some() {
                                return Err(de::Error::duplicate_field("status"));
                            }
                            status = Some(map.next_value::<String>()?);
                        }
                        "createdAt" => {
                            if created_at.is_some() {
                                return Err(de::Error::duplicate_field("createdAt"));
                            }
                            created_at = Some(map.next_value::<Option<String>>()?);
                        }
                        "terminatedAt" => {
                            if terminated_at.is_some() {
                                return Err(de::Error::duplicate_field("terminatedAt"));
                            }
                            terminated_at = Some(map.next_value::<Option<String>>()?);
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(CleanupTerminalSandbox {
                    id: id.ok_or_else(|| de::Error::missing_field("id"))?,
                    status: status.ok_or_else(|| de::Error::missing_field("status"))?,
                    created_at: created_at.flatten(),
                    terminated_at: terminated_at.flatten(),
                })
            }
        }
        deserializer.deserialize_map(TerminalVisitor)
    }
}

fn occurrence_from_create(
    sandbox: crate::RenderSandbox,
    body: &[u8],
    projection: &crate::provider_spec::BootstrapCreateProjection,
    operation_id: &str,
    requested_owner_scope: &str,
) -> RuntimeSnapshotBootstrapOccurrence {
    let verified = response_matches(&sandbox, projection);
    RuntimeSnapshotBootstrapOccurrence {
        schema: 1,
        operation_id: operation_id.to_owned(),
        occurrence_id: sandbox.id.clone(),
        provider_response_sha256: lillux::sha256_hex(body),
        provider_creation_observation: serde_json::json!({
            "created_at": sandbox.created_at,
            "status": sandbox.status,
            "plan": sandbox.plan,
            "region": sandbox.region,
            "timeout_seconds": sandbox.timeout_seconds,
            "network_policy": sandbox.network_policy,
            "terminated_at": sandbox.terminated_at,
            "requested_owner_scope": requested_owner_scope,
        }),
        creation_attributes_verified: verified,
        contact_deadline_exceeded: false,
    }
}

/// Read only a cleanup target from a bounded 201 body. This deliberately
/// tolerates new provider fields and enums but refuses duplicate `id` keys.
struct CleanupSandboxId {
    id: String,
}

impl<'de> Deserialize<'de> for CleanupSandboxId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct IdVisitor;
        impl<'de> Visitor<'de> for IdVisitor {
            type Value = CleanupSandboxId;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a Render response object with one sandbox id")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut id = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "id" {
                        if id.is_some() {
                            return Err(de::Error::duplicate_field("id"));
                        }
                        id = Some(map.next_value::<String>()?);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(CleanupSandboxId {
                    id: id.ok_or_else(|| de::Error::missing_field("id"))?,
                })
            }
        }
        deserializer.deserialize_map(IdVisitor)
    }
}

fn cleanup_only_occurrence(
    id: String,
    body: &[u8],
    operation_id: &str,
    requested_owner_scope: &str,
) -> RuntimeSnapshotBootstrapOccurrence {
    RuntimeSnapshotBootstrapOccurrence {
        schema: 1,
        operation_id: operation_id.to_owned(),
        occurrence_id: id,
        provider_response_sha256: lillux::sha256_hex(body),
        provider_creation_observation: serde_json::json!({
            "kind": "unverified_create_response",
            "requested_owner_scope": requested_owner_scope,
        }),
        creation_attributes_verified: false,
        contact_deadline_exceeded: false,
    }
}

fn uncertain(
    request: &RuntimeSnapshotBootstrapAdapterRequest,
    intent_digest: &str,
) -> RuntimeSnapshotBootstrapAdapterResponse {
    RuntimeSnapshotBootstrapAdapterResponse::Uncertain {
        operation_id: request.intent.operation_id.clone(),
        intent_digest: intent_digest.to_owned(),
    }
}

fn response_matches(
    sandbox: &crate::RenderSandbox,
    projection: &crate::provider_spec::BootstrapCreateProjection,
) -> bool {
    let expected_plan = match projection.plan {
        PlanValue::Starter => RenderSandboxPlan::Starter,
        PlanValue::Standard => RenderSandboxPlan::Standard,
        PlanValue::Pro => RenderSandboxPlan::Pro,
    };
    crate::valid_sandbox_id(&sandbox.id)
        && DateTime::parse_from_rfc3339(&sandbox.created_at).is_ok()
        && matches!(
            sandbox.status,
            RenderSandboxStatus::Creating | RenderSandboxStatus::Running
        )
        && sandbox.plan == expected_plan
        && sandbox.region == projection.region
        && sandbox.timeout_seconds == projection.timeout_seconds
        && sandbox.network_policy.default == crate::RenderNetworkPolicyDefault::DenyAll
        && sandbox.terminated_at.is_none()
}

fn readiness_running_matches(
    sandbox: &crate::RenderSandbox,
    occurrence: &RuntimeSnapshotBootstrapOccurrence,
    projection: &crate::provider_spec::BootstrapCreateProjection,
    now: i64,
) -> bool {
    let Ok(created) = DateTime::parse_from_rfc3339(&sandbox.created_at) else {
        return false;
    };
    let Some(expires) = created
        .timestamp_millis()
        .checked_add(i64::from(projection.timeout_seconds) * 1_000)
    else {
        return false;
    };
    sandbox.id == occurrence.occurrence_id
        && sandbox.status == RenderSandboxStatus::Running
        && now >= created.timestamp_millis()
        && now < expires
        && occurrence.provider_creation_observation["created_at"] == sandbox.created_at
        && response_matches(sandbox, projection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_response_binds_only_actual_exposed_attributes() {
        let sandbox: crate::RenderSandbox = from_json_slice_strict(
            include_bytes!("../fixtures/create-response.json"),
            usize::try_from(crate::MAX_API_RESPONSE_BYTES).unwrap(),
        )
        .unwrap();
        let projection = crate::provider_spec::BootstrapCreateProjection {
            owner_id: "owner-fixture".into(),
            plan: PlanValue::Standard,
            region: "oregon".into(),
            timeout_seconds: 600,
            network_policy_default: NetworkPolicyDefault::DenyAll,
        };
        assert!(response_matches(&sandbox, &projection));
        let mut changed = projection;
        changed.timeout_seconds += 1;
        assert!(!response_matches(&sandbox, &changed));
        let retained = occurrence_from_create(
            sandbox,
            include_bytes!("../fixtures/create-response.json"),
            &changed,
            &"a".repeat(64),
            "owner-fixture",
        );
        assert_eq!(retained.occurrence_id, "sbx-fixture-001");
        assert!(!retained.creation_attributes_verified);
        // Render does not expose account or group in this Sandbox response;
        // the callback must never report them as observed fields.
    }

    #[test]
    fn readiness_requires_same_created_occurrence_and_running_state() {
        let body = include_bytes!("../fixtures/create-response.json");
        let sandbox: crate::RenderSandbox = from_json_slice_strict(
            body,
            usize::try_from(crate::MAX_API_RESPONSE_BYTES).unwrap(),
        )
        .unwrap();
        let projection = crate::provider_spec::BootstrapCreateProjection {
            owner_id: "owner-fixture".into(),
            plan: PlanValue::Standard,
            region: "oregon".into(),
            timeout_seconds: 600,
            network_policy_default: NetworkPolicyDefault::DenyAll,
        };
        let source =
            occurrence_from_create(sandbox, body, &projection, &"a".repeat(64), "owner-fixture");
        let mut running: crate::RenderSandbox = from_json_slice_strict(
            body,
            usize::try_from(crate::MAX_API_RESPONSE_BYTES).unwrap(),
        )
        .unwrap();
        running.status = RenderSandboxStatus::Running;
        let during_lifetime = DateTime::parse_from_rfc3339(&running.created_at)
            .unwrap()
            .timestamp_millis()
            + 60_000;
        assert!(readiness_running_matches(
            &running,
            &source,
            &projection,
            during_lifetime
        ));
        assert!(!readiness_running_matches(
            &running,
            &source,
            &projection,
            during_lifetime + 600_000
        ));
        running.created_at = "2026-09-30T00:00:00Z".into();
        assert!(!readiness_running_matches(
            &running,
            &source,
            &projection,
            during_lifetime
        ));
        running.created_at = source.provider_creation_observation["created_at"]
            .as_str()
            .unwrap()
            .into();
        running.status = RenderSandboxStatus::Suspended;
        assert!(!readiness_running_matches(
            &running,
            &source,
            &projection,
            during_lifetime
        ));
    }

    #[test]
    fn unknown_provider_fields_keep_only_a_cleanup_target() {
        let body = br#"{"id":"sbx-fixture-001","status":"future_state","newField":true}"#;
        let id: CleanupSandboxId = from_json_slice_strict(
            body,
            usize::try_from(crate::MAX_API_RESPONSE_BYTES).unwrap(),
        )
        .unwrap();
        assert_eq!(id.id, "sbx-fixture-001");
        assert!(
            from_json_slice_strict::<crate::RenderSandbox>(
                body,
                usize::try_from(crate::MAX_API_RESPONSE_BYTES).unwrap()
            )
            .is_err()
        );
        let occurrence = cleanup_only_occurrence(id.id, body, &"a".repeat(64), "owner-fixture");
        assert!(!occurrence.creation_attributes_verified);
        assert_eq!(occurrence.occurrence_id, "sbx-fixture-001");
        let duplicate = br#"{"id":"sbx-fixture-001","id":"sbx-other"}"#;
        assert!(
            from_json_slice_strict::<CleanupSandboxId>(
                duplicate,
                usize::try_from(crate::MAX_API_RESPONSE_BYTES).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_cleanup_accepts_extra_fields_but_not_ambiguous_identity() {
        let body = br#"{"id":"sbx-fixture-001","status":"terminated","createdAt":"2026-09-29T00:00:00Z","terminatedAt":"2026-09-29T01:00:00Z","futureField":{"kind":"new"}}"#;
        let observed: CleanupTerminalSandbox = from_json_slice_strict(
            body,
            usize::try_from(crate::MAX_API_RESPONSE_BYTES).unwrap(),
        )
        .unwrap();
        assert_eq!(
            terminal_timestamp_for_cleanup(&observed, "sbx-fixture-001", "2026-09-29T00:00:00Z"),
            Some("2026-09-29T01:00:00Z")
        );
        assert!(
            terminal_timestamp_for_cleanup(&observed, "sbx-other", "2026-09-29T00:00:00Z")
                .is_none()
        );
        assert!(
            terminal_timestamp_for_cleanup(&observed, "sbx-fixture-001", "2026-09-28T00:00:00Z")
                .is_none()
        );
        let duplicate = br#"{"id":"sbx-fixture-001","status":"terminated","status":"running","terminatedAt":"2026-09-29T01:00:00Z"}"#;
        assert!(
            from_json_slice_strict::<CleanupTerminalSandbox>(
                duplicate,
                usize::try_from(crate::MAX_API_RESPONSE_BYTES).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn terminated_list_requires_exact_creation_and_unambiguous_bounded_pages() {
        let body = br#"[{"sandbox":{"id":"sbx-fixture-001","status":"terminated","createdAt":"2026-09-29T00:00:00Z","terminatedAt":"2026-09-29T01:00:00Z"},"cursor":"cursor-one"}]"#;
        let mut cursors = std::collections::BTreeSet::new();
        let mut found = None;
        assert_eq!(
            scan_terminated_page(
                body,
                "sbx-fixture-001",
                "2026-09-29T00:00:00Z",
                &mut cursors,
                &mut found,
            ),
            Some((1, Some("cursor-one".into())))
        );
        assert_eq!(found.as_ref().unwrap().0, lillux::sha256_hex(body));
        assert_eq!(found.as_ref().unwrap().1, "2026-09-29T01:00:00Z");
        assert!(
            scan_terminated_page(
                body,
                "sbx-fixture-001",
                "2026-09-29T00:00:00Z",
                &mut cursors,
                &mut found
            )
            .is_none()
        );

        let mut cursors = std::collections::BTreeSet::new();
        let mut found = None;
        assert!(
            scan_terminated_page(
                body,
                "sbx-fixture-001",
                "2026-09-28T00:00:00Z",
                &mut cursors,
                &mut found
            )
            .is_none()
        );
        assert!(found.is_none());
        let duplicate_field = br#"[{"sandbox":{"id":"sbx-fixture-001","id":"sbx-other","status":"terminated","createdAt":"2026-09-29T00:00:00Z","terminatedAt":"2026-09-29T01:00:00Z"},"cursor":"cursor-one"}]"#;
        assert!(
            scan_terminated_page(
                duplicate_field,
                "sbx-fixture-001",
                "2026-09-29T00:00:00Z",
                &mut cursors,
                &mut found
            )
            .is_none()
        );

        let full_page = (0..TERMINATED_LIST_PAGE_LIMIT)
            .map(|index| {
                serde_json::json!({
                    "sandbox": {
                        "id": if index == 0 { "sbx-fixture-001".to_owned() } else { format!("sbx-other-{index}") },
                        "status": "terminated",
                        "createdAt": "2026-09-29T00:00:00Z",
                        "terminatedAt": "2026-09-29T01:00:00Z"
                    },
                    "cursor": format!("cursor-{index}")
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&full_page).unwrap();
        let mut cursors = std::collections::BTreeSet::new();
        let mut found = None;
        let (count, _) = scan_terminated_page(
            &body,
            "sbx-fixture-001",
            "2026-09-29T00:00:00Z",
            &mut cursors,
            &mut found,
        )
        .unwrap();
        assert_eq!(count, TERMINATED_LIST_PAGE_LIMIT);
        assert!(found.is_some());
    }
}
